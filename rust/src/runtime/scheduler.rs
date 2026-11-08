use crate::api::sponsorblock::Segment;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub struct PlaybackSnapshot {
    pub video_id: String,
    pub state: i32,
    pub position: f64,
    pub speed: f64,
    pub observed: Instant,
    pub connection_epoch: u64,
    pub video_epoch: u64,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Suppression {
    pub connection_epoch: u64,
    pub video_epoch: u64,
    pub start: f64,
    pub end: f64,
}

pub fn next_skip(
    snapshot: &PlaybackSnapshot,
    segments: &[Segment],
    suppressed: &[Suppression],
    offset: f64,
    now: Instant,
) -> Option<(usize, Duration)> {
    if snapshot.state != 1
        || snapshot.video_id.is_empty()
        || !snapshot.speed.is_finite()
        || snapshot.speed <= 0.0
        || !snapshot.position.is_finite()
    {
        return None;
    }
    let elapsed = now
        .saturating_duration_since(snapshot.observed)
        .as_secs_f64();
    let position = snapshot.position + elapsed * snapshot.speed;
    if !position.is_finite() {
        return None;
    }
    segments.iter().enumerate().find_map(|(index, segment)| {
        if position >= segment.end
            || suppressed.iter().any(|marker| {
                marker.connection_epoch == snapshot.connection_epoch
                    && marker.video_epoch == snapshot.video_epoch
                    && marker.start == segment.start
                    && marker.end == segment.end
            })
        {
            return None;
        }
        let delay = if position >= segment.start {
            0.0
        } else {
            ((segment.start - snapshot.position) / snapshot.speed - elapsed - offset).max(0.0)
        };
        Duration::try_from_secs_f64(delay)
            .ok()
            .map(|delay| (index, delay))
    })
}

pub fn resolve_suppression(markers: &mut Vec<Suppression>, snapshot: &PlaybackSnapshot) {
    markers.retain(|marker| {
        marker.connection_epoch == snapshot.connection_epoch
            && marker.video_epoch == snapshot.video_epoch
            && snapshot.position < marker.end
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn playback(position: f64) -> PlaybackSnapshot {
        PlaybackSnapshot {
            video_id: "synthetic-video".into(),
            state: 1,
            position,
            speed: 1.0,
            observed: Instant::now(),
            connection_epoch: 1,
            video_epoch: 1,
            revision: 1,
        }
    }
    fn segment(start: f64, end: f64) -> Segment {
        Segment {
            start,
            end,
            uuids: vec![],
        }
    }
    #[test]
    fn zero_exact_start_and_later_segment_interior_seek_immediately() {
        for (position, start, end) in [(0.0, 0.0, 3.0), (10.0, 10.0, 15.0), (22.0, 20.0, 25.0)] {
            let state = playback(position);
            assert_eq!(
                next_skip(&state, &[segment(start, end)], &[], 0.0, state.observed),
                Some((0, Duration::ZERO))
            );
        }
    }
    #[test]
    fn speed_multiplies_elapsed_wall_time() {
        let mut state = playback(10.0);
        state.speed = 2.0;
        assert_eq!(
            next_skip(
                &state,
                &[segment(20.0, 25.0)],
                &[],
                0.2,
                state.observed + Duration::from_secs(1)
            ),
            Some((0, Duration::from_secs_f64(3.8)))
        );
    }
    #[test]
    fn nonplaying_states_and_invalid_speeds_cannot_seek() {
        let mut state = playback(0.0);
        for wire_state in [-1, 0, 2, 3, 5, 1081] {
            state.state = wire_state;
            assert!(next_skip(&state, &[segment(0.0, 3.0)], &[], 0.0, state.observed).is_none());
        }
        state.state = 1;
        for speed in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            state.speed = speed;
            assert!(next_skip(&state, &[segment(0.0, 3.0)], &[], 0.0, state.observed).is_none());
        }
    }
    #[test]
    fn observations_resolve_suppression_only_at_interval_end_or_new_identity() {
        let mut state = playback(1.0);
        let mut markers = vec![Suppression {
            connection_epoch: 1,
            video_epoch: 1,
            start: 0.0,
            end: 3.0,
        }];
        state.revision += 1;
        state.state = 2;
        resolve_suppression(&mut markers, &state);
        state.state = 1;
        state.speed = 2.0;
        resolve_suppression(&mut markers, &state);
        assert!(next_skip(&state, &[segment(0.0, 3.0)], &markers, 0.0, state.observed).is_none());
        state.position = 3.0;
        resolve_suppression(&mut markers, &state);
        state.position = 1.0;
        assert_eq!(
            next_skip(&state, &[segment(0.0, 3.0)], &markers, 0.0, state.observed),
            Some((0, Duration::ZERO))
        );
    }

    #[test]
    fn signed_offsets_adjust_future_deadlines_but_never_delay_an_inside_skip() {
        let state = playback(10.0);
        let segments = [segment(20.0, 25.0)];
        for (offset, delay) in [(-0.5, 10.5), (0.5, 9.5), (15.0, 0.0)] {
            assert_eq!(
                next_skip(&state, &segments, &[], offset, state.observed),
                Some((0, Duration::from_secs_f64(delay)))
            );
        }
        let inside = playback(21.0);
        assert_eq!(
            next_skip(&inside, &segments, &[], -0.5, inside.observed),
            Some((0, Duration::ZERO))
        );
    }

    #[test]
    fn elapsed_position_at_segment_end_selects_the_next_interval() {
        let mut state = playback(1.0);
        state.speed = 2.0;
        let segments = [segment(0.0, 3.0), segment(5.0, 8.0)];
        assert_eq!(
            next_skip(
                &state,
                &segments,
                &[],
                0.0,
                state.observed + Duration::from_secs(1),
            ),
            Some((1, Duration::from_secs(1)))
        );
        assert_eq!(
            next_skip(
                &state,
                &segments,
                &[],
                0.0,
                state.observed + Duration::from_secs(4),
            ),
            None
        );
    }

    #[test]
    fn suppression_skips_only_matching_interval_and_playback_identity() {
        let mut state = playback(1.0);
        let segments = [segment(0.0, 3.0), segment(5.0, 8.0)];
        let markers = [Suppression {
            connection_epoch: 1,
            video_epoch: 1,
            start: 0.0,
            end: 3.0,
        }];
        assert_eq!(
            next_skip(&state, &segments, &markers, 0.0, state.observed),
            Some((1, Duration::from_secs(4)))
        );
        for (connection_epoch, video_epoch) in [(2, 1), (1, 2)] {
            state.connection_epoch = connection_epoch;
            state.video_epoch = video_epoch;
            assert_eq!(
                next_skip(&state, &segments, &markers, 0.0, state.observed),
                Some((0, Duration::ZERO))
            );
        }
    }
}
