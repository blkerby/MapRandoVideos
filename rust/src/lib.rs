use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub enum EncodingTask {
    ThumbnailImage {
        video_id: i32,
        crop_center_x: i32,
        crop_center_y: i32,
        crop_size: i32,
        frame_number: i32
    },
    HighlightAnimation {
        video_id: i32,
        crop_center_x: i32,
        crop_center_y: i32,
        crop_size: i32,
        start_frame_number: i32,
        end_frame_number: i32,
    },
    FullVideo {
        video_id: i32,
    },
}
