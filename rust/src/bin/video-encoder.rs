use std::{
    io::Write,
    ops::Deref,
    process::{Command, Stdio},
};

use anyhow::{bail, Result};
use clap::Parser;
use futures::StreamExt;
use lapin::options::BasicQosOptions;
use log::info;
use map_rando_videos::{create_object_store, EncodingTask};
use object_store::{path::Path, ObjectStore};
use tokio::io::AsyncReadExt;

#[derive(Parser)]
struct Args {
    #[arg(long, env)]
    postgres_host: String,
    #[arg(long, env)]
    postgres_db: String,
    #[arg(long, env)]
    postgres_user: String,
    #[arg(long, env)]
    postgres_password: String,
    #[arg(long, env)]
    rabbit_url: String,
    #[arg(long, env)]
    rabbit_queue: String,
    #[arg(long, env)]
    video_storage_bucket_url: String,
    #[arg(long, env)]
    ffmpeg_path: String,
}

struct AppData {
    args: Args,
    db: deadpool_postgres::Pool,
    mq: deadpool_lapin::Pool,
    video_store: Box<dyn ObjectStore>,
}

async fn build_app_data() -> Result<AppData> {
    let args = Args::parse();

    // Open a Postgres database connection pool (for recording processed timestamps)
    let mut config = deadpool_postgres::Config::new();
    config.host = Some(args.postgres_host.clone());
    config.dbname = Some(args.postgres_db.clone());
    config.user = Some(args.postgres_user.clone());
    config.password = Some(args.postgres_password.clone());
    let db_pool = config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();

    // Create RabbitMQ connection pool
    let mut cfg = deadpool_lapin::Config::default();
    cfg.url = Some(args.rabbit_url.clone());
    let mq_pool = cfg.create_pool(Some(deadpool_lapin::Runtime::Tokio1))?;
    let mq = mq_pool.get().await?;
    let channel = mq.create_channel().await?;
    let mut opts = lapin::options::QueueDeclareOptions::default();
    opts.durable = true;
    channel
        .queue_declare(
            &args.rabbit_queue,
            opts,
            lapin::types::FieldTable::default(),
        )
        .await?;

    let app_data = AppData {
        db: db_pool,
        mq: mq_pool,
        video_store: create_object_store(&args.video_storage_bucket_url),
        args,
    };

    Ok(app_data)
}

async fn encode_thumbnail(
    app_data: &AppData,
    video_id: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    crop_size: i32,
    frame_number: i32,
) -> Result<()> {
    let input_path = Path::parse(format!("avi-xz/{}.avi.xz", video_id))?;
    let compressed_input = app_data.video_store.get(&input_path).await?.bytes().await?;
    let mut uncompressed_input =
        async_compression::tokio::bufread::XzDecoder::new(compressed_input.deref());
    let pipe_path = "/tmp/video.pipe";
    let output_path = "/tmp/thumbnail.png";

    let _ = std::fs::remove_file(pipe_path);
    unix_named_pipe::create(pipe_path, Some(0o644))?;

    let crop_x = crop_center_x - crop_size / 2;
    let crop_y = crop_center_y - crop_size / 2;

    // Run ffmpeg to extract and crop a single selected frame from the video:
    let mut child = Command::new(&app_data.args.ffmpeg_path)
        .arg("-y")
        .arg("-i")
        .arg(pipe_path)
        .arg("-vf")
        .arg(&format!(
            "select=eq(n\\, {frame_number}),crop={crop_size}:{crop_size}:{crop_x}:{crop_y}"
        ))
        .arg("-vframes")
        .arg("1")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("error spawning ffmpeg");

    // Pipe the uncompressed AVI into ffmpeg via named pipe:
    let mut pipe = std::fs::OpenOptions::new().write(true).open(pipe_path)?;
    let mut buf = vec![0u8; 65536];
    loop {
        let n = uncompressed_input.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if pipe.write_all(&buf[..n]).is_err() {
            // We ignore errors writing to the pipe.
            // A broken pipe is expected since ffmpeg closes the input before reading to the end.
            break;
        }
    }
    drop(pipe);

    let status = child.wait()?;
    info!("ffmpeg {}", status);

    if !status.success() {
        bail!("ffmpeg returned non-zero status");
    }

    // Write the output thumbnail to object storage:
    let output_data = std::fs::read(output_path)?;
    let output_key = object_store::path::Path::parse(format!("png/{}.png", video_id))?;
    app_data
        .video_store
        .put(&output_key, output_data.into())
        .await?;

    // Update the `thumbnail_processed_ts` in the database:
    let db = app_data.db.get().await?;
    let sql = "UPDATE video SET thumbnail_processed_ts=current_timestamp WHERE id=$1";
    let stmt = db.prepare_cached(&sql).await?;
    db.execute(&stmt, &[&video_id]).await?;

    Ok(())
}

async fn encode_highlight(
    app_data: &AppData,
    video_id: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    crop_size: i32,
    start_frame_number: i32,
    end_frame_number: i32,
) -> Result<()> {
    let input_path = Path::parse(format!("avi-xz/{}.avi.xz", video_id))?;
    let compressed_input = app_data.video_store.get(&input_path).await?.bytes().await?;
    let mut uncompressed_input =
        async_compression::tokio::bufread::XzDecoder::new(compressed_input.deref());
    let pipe_path = "/tmp/video.pipe";
    let output_path = "/tmp/highlight.webp";

    let _ = std::fs::remove_file(pipe_path);
    unix_named_pipe::create(pipe_path, Some(0o644))?;

    let crop_x = crop_center_x - crop_size / 2;
    let crop_y = crop_center_y - crop_size / 2;

    // Run ffmpeg to extract and crop a selected range of frames from the video, cutting the frame rate by a factor of 3:
    let mut child = Command::new(&app_data.args.ffmpeg_path)
        .arg("-y")
        .arg("-i")
        .arg(pipe_path)
        .arg("-vf")
        .arg(&format!(
            "select='between(n\\, {start_frame_number}, {end_frame_number})*not(mod(n-{start_frame_number}\\,3))',crop={crop_size}:{crop_size}:{crop_x}:{crop_y}"
        ))
        .arg("-c:v")
        .arg("libwebp_anim")
        .arg("-lossless")
        .arg("1")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("error spawning ffmpeg");

    // Pipe the uncompressed AVI into ffmpeg via named pipe:
    let mut pipe = std::fs::OpenOptions::new().write(true).open(pipe_path)?;
    let mut buf = vec![0u8; 65536];
    loop {
        let n = uncompressed_input.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if let Err(e) = pipe.write_all(&buf[..n]) {
            // In case of error writing to the pipe, we log it but don't crash.
            // A broken pipe is expected since ffmpeg closes the input before reading to the end.
            info!("Error writing to pipe: {}", e);
            break;
        }
    }
    drop(pipe);

    let status = child.wait()?;
    info!("ffmpeg {}", status);

    if !status.success() {
        bail!("ffmpeg returned non-zero status");
    }

    // Write the output highlight to object storage:
    let output_data = std::fs::read(output_path)?;
    let output_key = object_store::path::Path::parse(format!("webp/{}.webp", video_id))?;
    app_data
        .video_store
        .put(&output_key, output_data.into())
        .await?;

    // Update the `highlight_processed_ts` in the database:
    let db = app_data.db.get().await?;
    let sql = "UPDATE video SET highlight_processed_ts=current_timestamp WHERE id=$1";
    let stmt = db.prepare_cached(&sql).await?;
    db.execute(&stmt, &[&video_id]).await?;

    Ok(())
}

async fn encode_full_video(
    app_data: &AppData,
    video_id: i32,
) -> Result<()> {
    let input_path = Path::parse(format!("avi-xz/{}.avi.xz", video_id))?;
    let compressed_input = app_data.video_store.get(&input_path).await?.bytes().await?;
    let mut uncompressed_input =
        async_compression::tokio::bufread::XzDecoder::new(compressed_input.deref());
    let pipe_path = "/tmp/video.pipe";
    let output_path = "/tmp/full_video.mp4";

    let _ = std::fs::remove_file(pipe_path);
    unix_named_pipe::create(pipe_path, Some(0o644))?;

    // Run ffmpeg to encode the video into an mp4. For best compatibility, we use yuv420p pixel format;
    // this subsamples the chroma, which we counteract by upscaling the video resolution by 2x.
    let mut child = Command::new(&app_data.args.ffmpeg_path)
        .arg("-y")
        .arg("-i")
        .arg(pipe_path)
        .arg("-vf")
        .arg("scale=512:-1:flags=neighbor")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-preset")
        .arg("veryslow")
        .arg("-crf")
        .arg("23")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("error spawning ffmpeg");

    // Pipe the uncompressed AVI into ffmpeg via named pipe:
    let mut pipe = std::fs::OpenOptions::new().write(true).open(pipe_path)?;
    let mut buf = vec![0u8; 65536];
    loop {
        let n = uncompressed_input.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if let Err(e) = pipe.write_all(&buf[..n]) {
            // In case of error writing to the pipe, we log it but don't crash.
            // A broken pipe is expected since ffmpeg closes the input before reading to the end.
            info!("Error writing to pipe: {}", e);
            break;
        }
    }
    drop(pipe);

    let status = child.wait()?;
    info!("ffmpeg {}", status);

    if !status.success() {
        bail!("ffmpeg returned non-zero status");
    }

    // Write the output mp4 to object storage:
    let output_data = std::fs::read(output_path)?;
    let output_key = object_store::path::Path::parse(format!("mp4/{}.mp4", video_id))?;
    app_data
        .video_store
        .put(&output_key, output_data.into())
        .await?;

    // Update the `full_video_processed_ts` in the database:
    let db = app_data.db.get().await?;
    let sql = "UPDATE video SET full_video_processed_ts=current_timestamp WHERE id=$1";
    let stmt = db.prepare_cached(&sql).await?;
    db.execute(&stmt, &[&video_id]).await?;

    Ok(())
}

async fn process_task(task: &EncodingTask, app_data: &AppData) -> Result<()> {
    match task {
        &EncodingTask::ThumbnailImage {
            video_id,
            crop_center_x,
            crop_center_y,
            crop_size,
            frame_number,
        } => {
            encode_thumbnail(
                &app_data,
                video_id,
                crop_center_x,
                crop_center_y,
                crop_size,
                frame_number,
            )
            .await?;
        }
        &EncodingTask::HighlightAnimation {
            video_id,
            crop_center_x,
            crop_center_y,
            crop_size,
            start_frame_number,
            end_frame_number,
        } => {
            encode_highlight(
                app_data,
                video_id,
                crop_center_x,
                crop_center_y,
                crop_size,
                start_frame_number,
                end_frame_number,
            )
            .await?;
        }
        &EncodingTask::FullVideo { video_id } => {
            encode_full_video(app_data, video_id).await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();
    let app_data = build_app_data().await?;
    let mq = app_data.mq.get().await?;
    let channel = mq.create_channel().await?;
    let opts = lapin::options::BasicConsumeOptions::default();
    channel.basic_qos(1, BasicQosOptions::default()).await?;
    let mut consumer = channel
        .basic_consume(
            &args.rabbit_queue,
            "video-encoder",
            opts,
            lapin::types::FieldTable::default(),
        )
        .await?;
    info!("Waiting for messages");
    while let Some(delivery) = consumer.next().await {
        let delivery = delivery?;
        info!(
            "Consuming message: {}",
            String::from_utf8(delivery.data.clone())?
        );
        let task: EncodingTask = serde_json::from_slice(&delivery.data)?;
        process_task(&task, &app_data).await?;
        delivery
        .ack(lapin::options::BasicAckOptions::default())
        .await?;
    }
    bail!("Consumer unexpectedly finished");
}