use crate::{
    error::{AdjustPartDurationError, SegmenterWriteH264Error},
    playlist::Playlist,
    segment::Segment,
    types::IdCounter,
};
use common::{
    time::{DurationH264, UnixH264, H264_MILLISECOND, H264_SECOND},
    H264Data, VideoSample,
};
use std::{collections::HashSet, sync::Arc};
use tokio_util::sync::DropGuard;

// Opaque wrapper around segmenter that will cancel the muxer when dropped.
pub struct H264Writer {
    segmenter: Segmenter,
    _guard: DropGuard,
}

impl H264Writer {
    #[must_use]
    pub fn new(segmenter: Segmenter, guard: DropGuard) -> Self {
        Self {
            segmenter,
            _guard: guard,
        }
    }
    pub async fn write_h264(&mut self, data: H264Data) -> Result<(), SegmenterWriteH264Error> {
        self.segmenter.write_h264(data).await
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    pub async fn test_write(&mut self, pts: i64, avcc: Vec<u8>, random_access: bool) {
        use sentryshot_padded_bytes::PaddedBytes;

        self.write_h264(H264Data {
            pts: DurationH264::new(pts),
            dts: DurationH264::new(pts),
            avcc: Arc::new(PaddedBytes::new(avcc)),
            random_access_present: random_access,
        })
        .await
        .unwrap();
    }
}

pub struct Segmenter {
    segment_duration: DurationH264,
    part_duration: DurationH264,
    segment_max_size: u64,
    playlist: Arc<Playlist>,
    muxer_id: u16,

    muxer_start_time: i64,
    //last_video_params: Vec<Vec<u8>>,
    current_segment: Option<Segment>,
    segment_id_counter: IdCounter,
    part_id_counter: IdCounter,
    next_sample: Option<VideoSample>,
    first_segment_finalized: bool,
    sample_durations: HashSet<DurationH264>,
    adjusted_part_duration: DurationH264,
}

impl Segmenter {
    pub fn new(
        muxer_start_time: i64,
        segment_duration: DurationH264,
        part_duration: DurationH264,
        segment_max_size: u64,
        playlist: Arc<Playlist>,
        muxer_id: u16,
    ) -> Self {
        Self {
            segment_duration,
            part_duration,
            segment_max_size,
            playlist,
            muxer_start_time,
            muxer_id,
            //last_video_params: Vec::new(),
            current_segment: None,
            segment_id_counter: IdCounter::new(7), // 7 required by iOS.
            part_id_counter: IdCounter::new(0),
            next_sample: None,
            first_segment_finalized: false,
            sample_durations: HashSet::new(),
            adjusted_part_duration: DurationH264::from(0),
        }
    }

    // iPhone iOS fails if part durations are less than 85% of maximum part duration.
    // find a part duration that is compatible with all received sample durations.
    fn adjust_part_duration(&mut self, du: DurationH264) -> Result<(), AdjustPartDurationError> {
        if self.first_segment_finalized {
            return Ok(());
        }

        // Skip invalid durations.
        if *du == 0 {
            return Ok(());
        }

        if !self.sample_durations.contains(&du) {
            self.sample_durations.insert(du);
            self.adjusted_part_duration =
                find_compatible_part_duration(self.part_duration, &self.sample_durations)
                    .ok_or(AdjustPartDurationError::Error)?;
        }
        Ok(())
    }

    // First packet must be an IDR.
    pub async fn write_h264(&mut self, data: H264Data) -> Result<(), SegmenterWriteH264Error> {
        use crate::error::SegmenterWriteH264Error::*;
        let sample = VideoSample {
            ntp: UnixH264::now(),
            pts: data.pts,
            dts: data.dts,
            avcc: data.avcc,
            random_access_present: data.random_access_present,
            duration: DurationH264::from(0),
        };

        let next_dts = sample.dts;

        // Put samples in a queue in order to compute sample duration.
        let Some(mut sample) = self.next_sample.replace(sample) else {
            // Return if next_sample is None.
            return Ok(());
        };

        let sample_ntp = sample.ntp;
        let sample_dts = sample.dts;

        sample.duration = next_dts
            .checked_sub(sample_dts)
            .ok_or(ComputeSampleDuration)?;
        if *sample.duration < 0 {
            sample.duration = DurationH264::from(0);
        }

        self.adjust_part_duration(sample.duration)?;

        let current_segment = {
            if let Some(current_segment) = &mut self.current_segment {
                current_segment
            } else {
                // create first segment.
                self.current_segment.insert(Segment::new(
                    self.segment_id_counter.next_id(),
                    self.muxer_id,
                    sample_ntp,
                    sample_dts,
                    self.muxer_start_time,
                    self.segment_max_size,
                    self.playlist.clone(),
                    &mut self.part_id_counter,
                ))
            }
        };

        let segment_start_dts = current_segment.start_dts();
        current_segment
            .write_h264(
                sample,
                self.adjusted_part_duration,
                &mut self.part_id_counter,
            )
            .await?;

        // switch segment
        if data.random_access_present {
            //videoParams := extractVideoParams(m.videoTrack)
            //paramsChanged := !videoParamsEqual(m.lastVideoParams, videoParams)

            if next_dts
                .checked_sub(segment_start_dts)
                .ok_or(SwitchSegment)?
                >= self.segment_duration
            /*|| paramsChanged*/
            {
                let next_segment = Segment::new(
                    self.segment_id_counter.next_id(),
                    self.muxer_id,
                    sample_ntp,
                    sample_dts,
                    self.muxer_start_time,
                    self.segment_max_size,
                    self.playlist.clone(),
                    &mut self.part_id_counter,
                );
                let prev_segment = std::mem::replace(current_segment, next_segment);

                let Some(finalized_segment) = prev_segment.finalize(next_dts).await? else {
                    // Cancelled.
                    return Ok(());
                };
                self.playlist.on_segment_finalized(finalized_segment).await;

                self.first_segment_finalized = true;

                /*if paramsChanged {
                    m.lastVideoParams = videoParams
                    m.firstSegmentFinalized = false

                    // reset adjusted part duration
                    m.sampleDurations = make(map[time.Duration]struct{})
                }*/
            }
        }

        Ok(())
    }
}

/*
fn extractVideoParams(track: *gortsplib.TrackH264) ->  Vec<Vec<u8>> {
    params := make([][]byte, 2);
    params[0] = track.SafeSPS();
    params[1] = track.SafePPS();
    return params
}

fn videoParamsEqual(p1: Vec<Vec<u8>>, p2: Vec<Vec<u8>>)-> bool {
    if len(p1) != len(p2) {
        return true
    }

    for i, p := range p1 {
        if !bytes.Equal(p2[i], p) {
            return false
        }
    }
    return true
}
*/

fn part_duration_is_compatible(
    part_duration: DurationH264,
    sample_duration: DurationH264,
) -> Option<bool> {
    if sample_duration > part_duration {
        return Some(false);
    }

    let mut f = part_duration.checked_div(sample_duration)?;
    if !(part_duration.checked_rem(sample_duration)?).is_zero() {
        f = f.checked_add(DurationH264::from(1))?;
    }
    f = f.checked_mul(sample_duration)?;

    Some(
        part_duration
            > f.checked_mul(DurationH264::from(85))?
                .checked_div(DurationH264::from(100))?,
    )
}

fn part_duration_is_compatible_with_all(
    part_duration: DurationH264,
    sample_durations: &HashSet<DurationH264>,
) -> Option<bool> {
    for sd in sample_durations {
        if !part_duration_is_compatible(part_duration, *sd)? {
            return Some(false);
        }
    }
    Some(true)
}

fn find_compatible_part_duration(
    min_part_duration: DurationH264,
    sample_durations: &HashSet<DurationH264>,
) -> Option<DurationH264> {
    let mut i = min_part_duration;
    while *i < 5 * H264_SECOND {
        if part_duration_is_compatible_with_all(i, sample_durations)? {
            break;
        }
        i = i.checked_add(DurationH264::new(5 * H264_MILLISECOND))?;
    }
    Some(i)
}
