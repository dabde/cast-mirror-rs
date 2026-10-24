use crate::constants::{
    MAXIMUM_AUDIO_SYNC_SOURCE, MAXIMUM_VIDEO_SYNC_SOURCE, MINIMUM_AUDIO_SYNC_SOURCE,
    MINIMUM_VIDEO_SYNC_SOURCE,
};
use rand::Rng;

pub type SyncSource = u32;

pub fn generate_audio() -> SyncSource {
    rand::thread_rng().gen_range(MINIMUM_AUDIO_SYNC_SOURCE..=MAXIMUM_AUDIO_SYNC_SOURCE)
}

pub fn generate_video() -> SyncSource {
    rand::thread_rng().gen_range(MINIMUM_VIDEO_SYNC_SOURCE..=MAXIMUM_VIDEO_SYNC_SOURCE)
}

pub fn is_audio(sync_source: SyncSource) -> bool {
    (MINIMUM_AUDIO_SYNC_SOURCE..=MAXIMUM_AUDIO_SYNC_SOURCE).contains(&sync_source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_ssrc_in_range() {
        for _ in 0..100 {
            let id = generate_audio();
            assert!((MINIMUM_AUDIO_SYNC_SOURCE..=MAXIMUM_AUDIO_SYNC_SOURCE).contains(&id));
            assert!(is_audio(id));
        }
    }

    #[test]
    fn video_ssrc_in_range() {
        for _ in 0..100 {
            let id = generate_video();
            assert!((MINIMUM_VIDEO_SYNC_SOURCE..=MAXIMUM_VIDEO_SYNC_SOURCE).contains(&id));
            assert!(!is_audio(id));
        }
    }
}
