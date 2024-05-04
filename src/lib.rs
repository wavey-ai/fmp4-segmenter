use au::{
    AuKind::{AAC, AVC},
    AuPayload, Fmp4,
};
use bytes::{Buf, Bytes};
use h264::{Bitstream, Decode, NALUnit, SequenceParameterSet};
use mse_fmp4::io::WriteTo;
use mse_fmp4::{
    aac::{AacProfile, AdtsHeader, ChannelConfiguration, SamplingFrequency},
    avc::AvcDecoderConfigurationRecord,
    fmp4::{
        AacSampleEntry, AvcConfigurationBox, AvcSampleEntry, InitializationSegment, MediaDataBox,
        MediaSegment, MovieExtendsHeaderBox, Mp4Box, Mpeg4EsDescriptorBox, Sample, SampleEntry,
        SampleFlags, TrackBox, TrackExtendsBox, TrackFragmentBox,
    },
};
use std::io::Write;
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc::Sender;

use tracing::{debug, error, info};

pub fn ms_to_hz(v: u32, hz: u32) -> u32 {
    // Convert milliseconds to secods as f64 to preserve the fractional part
    let seconds = v as f64 / 1000.0;
    // Convert seconds to 90kHz clock ticks
    (seconds * hz as f64) as u32
}

pub fn ms_to_90khz_ticks(v: u32) -> u64 {
    // Convert milliseconds to seconds as f64 to preserve the fractional part
    let seconds = v as f64 / 1000.0;

    // Convert seconds to 90kHz clock ticks
    (seconds * 90000.0) as u64
}

pub struct Segmenter {
    audio_only: bool,
}

impl Segmenter {
    pub fn new(audio_only: bool) -> Self {
        Self { audio_only }
    }

    pub async fn start(&mut self, min_part_ms: u32, mut rx: Receiver<AuPayload>, tx: Sender<Fmp4>) {
        let mut avc_buf: Vec<AuPayload> = Vec::new();
        let mut aac_buf: Vec<AuPayload> = Vec::new();
        let mut avc_timestamps = Vec::new();
        let mut aac_timestamps = Vec::new();
        let mut avcc: Option<AvcDecoderConfigurationRecord> = None;
        let mut seq = 1;

        while let Ok(p) = rx.recv().await {
            let pts = p.pts();
            let dts = p.dts();

            if avcc.is_none() {
                if let Some(sps_b) = &p.sps {
                    if let Some(pps_b) = &p.pps {
                        let bs = Bitstream::new(sps_b.iter().copied());
                        match NALUnit::decode(bs) {
                            Ok(mut nalu) => {
                                let mut rbsp = Bitstream::new(&mut nalu.rbsp_byte);
                                if let Ok(sps) = SequenceParameterSet::decode(&mut rbsp) {
                                    avcc = Some(AvcDecoderConfigurationRecord {
                                        profile_idc: sps.profile_idc.0,
                                        constraint_set_flag: sps.constraint_set0_flag.0,
                                        level_idc: sps.level_idc.0,
                                        sequence_parameter_set: sps_b.clone(),
                                        picture_parameter_set: pps_b.clone(),
                                    });
                                };
                            }
                            Err(e) => {
                                dbg!(e);
                            }
                        }
                    }
                }
            }

            match p.kind {
                AAC => {
                    if self.audio_only {
                        if aac_timestamps.len() > 1 {
                            let a = aac_timestamps[aac_timestamps.len() - 1] as u32;
                            let b = aac_timestamps[0] as u32;
                            if a > b {
                                let elapsed_ms = a - b;

                                if elapsed_ms >= min_part_ms {
                                    let mut avc = Vec::new();
                                    let mut aac = Vec::new();
                                    for a in &avc_buf {
                                        avc.push(a.clone())
                                    }
                                    for a in &aac_buf {
                                        aac.push(a.clone())
                                    }

                                    let fmp4 = self.box_fmp4(seq, None, avc, aac, pts);
                                    seq += 1;
                                    match tx.send(fmp4).await {
                                        Ok(_) => {}
                                        Err(e) => eprintln!("error sending fmp4: {}", e),
                                    };

                                    aac_buf.clear();
                                    aac_timestamps.clear();
                                }
                            }
                        }
                        aac_timestamps.push(pts);
                        aac_buf.push(p);
                    } else {
                        aac_buf.push(p);
                    }
                }
                AVC => {
                    if avc_timestamps.len() > 1 {
                        let elapsed_ms = avc_timestamps[avc_timestamps.len() - 1] as u32
                            - avc_timestamps[0] as u32;

                        if elapsed_ms >= min_part_ms || p.key {
                            let mut avc = Vec::new();
                            let mut aac = Vec::new();
                            for a in &avc_buf {
                                avc.push(a.clone());
                            }
                            for a in &aac_buf {
                                aac.push(a.clone());
                            }

                            if let Some(c) = avcc.clone() {
                                let fmp4 = self.box_fmp4(seq, Some(c), avc, aac, dts);
                                seq += 1;
                                match tx.send(fmp4).await {
                                    Ok(_) => {}
                                    Err(e) => eprintln!("error sending fmp4: {}", e),
                                };
                            }

                            avc_buf.clear();
                            aac_buf.clear();
                            avc_timestamps.clear();
                        }
                    }
                    avc_timestamps.push(dts);
                    avc_buf.push(p);
                }
                _ => {}
            }
        }
    }

    pub fn box_fmp4(
        &mut self,
        seq: u32,
        avcc: Option<AvcDecoderConfigurationRecord>,
        avcs: Vec<AuPayload>,
        aacs: Vec<AuPayload>,
        next_dts: i64,
    ) -> Fmp4 {
        let mut segment = MediaSegment::new(seq);
        let mut fmp4_data: Vec<u8> = Vec::new();
        let mut init_data: Vec<u8> = Vec::new();
        let mut is_key = if self.audio_only { true } else { false };
        let mut total_ms = 0;

        let mut avc_data = Vec::new();
        let mut aac_data = Vec::new();

        let mut avc_samples = Vec::new();
        let mut aac_samples = Vec::new();

        if let Some(a) = &avcc {
            let mut avc_total_ms = 0;
            let mut avc_timestamps = Vec::new();

            for a in avcs.iter() {
                if a.key {
                    is_key = true;
                }
                let pts = a.pts();
                let dts = a.dts();

                if let Some(data) = &a.data {
                    let prev_data_len = &avc_data.len();
                    avc_data.extend_from_slice(&data);
                    let sample_size = (avc_data.len() - prev_data_len) as u32;
                    let sample_composition_time_offset = (pts - dts) as i32;

                    avc_timestamps.push(dts);

                    let flags = if a.key {
                        Some(SampleFlags {
                            is_leading: 0,
                            sample_depends_on: 0,
                            sample_is_depdended_on: 0,
                            sample_has_redundancy: 0,
                            sample_padding_value: 0,
                            sample_is_non_sync_sample: false,
                            sample_degradation_priority: 0,
                        })
                    } else {
                        Some(SampleFlags {
                            is_leading: 0,
                            sample_depends_on: 1,
                            sample_is_depdended_on: 0,
                            sample_has_redundancy: 0,
                            sample_padding_value: 0,
                            sample_is_non_sync_sample: true,
                            sample_degradation_priority: 0,
                        })
                    };

                    avc_samples.push(Sample {
                        duration: None,
                        size: Some(sample_size),
                        flags,
                        composition_time_offset: Some(sample_composition_time_offset),
                    });
                }
            }

            avc_timestamps.push(next_dts);
            for i in 0..avc_samples.len() {
                let duration = avc_timestamps[i + 1] - avc_timestamps[i];
                total_ms += duration as u32;
                avc_samples[i].duration = Some(ms_to_90khz_ticks(duration as u32) as u32);
            }

            let mut traf = TrackFragmentBox::new(true);
            traf.trun_box.first_sample_flags = None;
            traf.tfhd_box.default_sample_flags = None;
            traf.trun_box.data_offset = Some(0);
            traf.trun_box.samples = avc_samples;
            traf.tfdt_box.base_media_decode_time = ms_to_90khz_ticks(avcs[0].dts() as u32) as u32;
            segment.moof_box.traf_boxes.push(traf);
        }

        let mut aac_set = false;
        let mut frame_duration = 0;
        let mut sampling_frequency =
            SamplingFrequency::from_frequency(0).unwrap_or_else(|_| SamplingFrequency::Hz48000);
        let mut channel_configuration =
            ChannelConfiguration::from_u8(0).unwrap_or_else(|_| ChannelConfiguration::TwoChannels);
        let mut profile = AacProfile::Main;

        for a in aacs.iter() {
            if !aac_set {
                if let (Some(sr), Some(ac)) = (a.audio_sample_rate, a.audio_channels) {
                    let adts_header = AdtsHeader {
                        sampling_frequency: SamplingFrequency::from_frequency(sr)
                            .unwrap_or_else(|_| SamplingFrequency::Hz48000),
                        channel_configuration: ChannelConfiguration::from_u8(
                            a.audio_channels.unwrap_or(ac),
                        )
                        .unwrap_or_else(|_| ChannelConfiguration::TwoChannels),
                        profile: AacProfile::Main,
                        frame_len: 0,
                        buffer_fullness: 0,
                        private: false,
                    };
                    sampling_frequency = adts_header.sampling_frequency;
                    channel_configuration = adts_header.channel_configuration;
                    profile = adts_header.profile;

                    frame_duration = ((1024 as f32
                        / adts_header.sampling_frequency.as_u32() as f32)
                        * 1000 as f32)
                        .round() as u32;

                    aac_set = true;
                } else {
                    if let Some(data) = &a.data {
                        match AdtsHeader::read_from(&data[..]) {
                            Ok(adts_header) => {
                                sampling_frequency = adts_header.sampling_frequency;
                                channel_configuration = adts_header.channel_configuration;
                                profile = adts_header.profile;
                                frame_duration =
                                    ((1024 as f32 / sampling_frequency.as_u32() as f32)
                                        * 1000 as f32)
                                        .round() as u32;
                                aac_set = true;
                            }
                            Err(e) => {
                                eprintln!("Error decoding aac: {}", e)
                            }
                        }
                    }
                }
            }

            if let Some(data) = &a.data {
                if let Some(sr) = a.audio_sample_rate {
                    aac_samples.push(Sample {
                        duration: Some(ms_to_hz(frame_duration, sampling_frequency.as_u32())),
                        size: Some(data.len() as u32),
                        flags: None,
                        composition_time_offset: None,
                    });
                    aac_data.extend_from_slice(&data);
                } else {
                    let mut bytes = &data[..];
                    while !bytes.is_empty() {
                        if let Ok(header) = AdtsHeader::read_from(&mut bytes) {
                            let sample_size = header.raw_data_blocks_len();
                            aac_samples.push(Sample {
                                duration: Some(ms_to_hz(
                                    frame_duration,
                                    header.sampling_frequency.as_u32(),
                                )),
                                size: Some(u32::from(sample_size)),
                                flags: None,
                                composition_time_offset: None,
                            });
                            aac_data.extend_from_slice(&bytes[..sample_size as usize]);
                            bytes = &bytes[sample_size as usize..];
                        }
                    }
                }
            }
        }

        if avc_data.len() > 0 {
            segment.add_track_data(0, &avc_data);
        }

        let mut audio_track = TrackBox::new(false);
        if !aacs.is_empty() {
            let mut traf = TrackFragmentBox::new(false);
            traf.tfhd_box.default_sample_duration = None;
            traf.trun_box.data_offset = Some(0);
            traf.trun_box.samples = aac_samples;
            traf.tfdt_box.base_media_decode_time =
                ms_to_hz(aacs[0].pts() as u32, sampling_frequency.as_u32());
            segment.moof_box.traf_boxes.push(traf);

            segment.add_track_data(1, &aac_data);

            audio_track.tkhd_box.duration = 0;
            audio_track.mdia_box.mdhd_box.timescale = sampling_frequency.as_u32();
            audio_track.mdia_box.mdhd_box.duration = 0;

            let aac_sample_entry = AacSampleEntry {
                esds_box: Mpeg4EsDescriptorBox {
                    profile,
                    frequency: sampling_frequency,
                    channel_configuration,
                },
            };
            audio_track
                .mdia_box
                .minf_box
                .stbl_box
                .stsd_box
                .sample_entries
                .push(SampleEntry::Aac(aac_sample_entry));
        }
        segment.update_offsets();
        segment.write_to(&mut fmp4_data).unwrap();

        // create init.mp4
        let mut segment = InitializationSegment::default();
        segment.moov_box.mvhd_box.timescale = 1000;
        segment.moov_box.mvhd_box.duration = 0;
        segment.moov_box.mvex_box.mehd_box = Some(MovieExtendsHeaderBox {
            fragment_duration: 0,
        });

        if let Some(c) = avcc {
            let width = avcs[0].width.unwrap();
            let height = avcs[0].height.unwrap();

            // video track
            let mut track = TrackBox::new(true);
            track.tkhd_box.width = (width as u32) << 16;
            track.tkhd_box.height = (height as u32) << 16;
            track.tkhd_box.duration = 0;
            //track.edts_box.elst_box.media_time = start_time;
            track.mdia_box.mdhd_box.timescale = 90000;
            track.mdia_box.mdhd_box.duration = 0;

            let avc_sample_entry = AvcSampleEntry {
                width,
                height,
                avcc_box: AvcConfigurationBox {
                    configuration: c.clone(),
                },
            };
            track
                .mdia_box
                .minf_box
                .stbl_box
                .stsd_box
                .sample_entries
                .push(SampleEntry::Avc(avc_sample_entry));
            segment.moov_box.trak_boxes.push(track);
            segment
                .moov_box
                .mvex_box
                .trex_boxes
                .push(TrackExtendsBox::new(true));
        }

        if aacs.len() > 0 {
            // audio track
            segment.moov_box.trak_boxes.push(audio_track);
            segment
                .moov_box
                .mvex_box
                .trex_boxes
                .push(TrackExtendsBox::new(false));
        }

        let _ = segment.write_to(&mut init_data);

        let mut init: Option<Bytes> = None;
        if !init_data.is_empty() {
            init = Some(Bytes::from(init_data))
        }

        if total_ms == 0 {
            total_ms = frame_duration * aacs.len() as u32;
        }

        Fmp4 {
            init,
            duration: total_ms as u32,
            key: is_key,
            data: Bytes::from(fmp4_data),
        }
    }
}
