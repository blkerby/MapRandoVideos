use std::path::Path;

use object_store::{aws::AmazonS3Builder, gcp::GoogleCloudStorageBuilder, local::LocalFileSystem, memory::InMemory, ObjectStore};
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

pub fn create_object_store(url: &str) -> Box<dyn ObjectStore> {
    let object_store: Box<dyn ObjectStore> = if url.starts_with("gs:") {
        Box::new(
            GoogleCloudStorageBuilder::from_env()
                .with_url(url)
                .build()
                .unwrap(),
        )
    } else if url.starts_with("s3:") {
        let bucket = &url[3..];
        Box::new(
            AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .build()
                .unwrap(),
        )
    } else if url == "mem" {
        Box::new(InMemory::new())
    } else if url.starts_with("file:") {
        let root = &url[5..];
        Box::new(LocalFileSystem::new_with_prefix(Path::new(root)).unwrap())
    } else {
        panic!("Unsupported seed repository type: {}", url);
    };
    object_store
}
