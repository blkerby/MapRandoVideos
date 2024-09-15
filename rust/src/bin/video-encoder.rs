use std::{
    fs::File, io::Write, ops::Deref, process::{Command, Stdio}
};

use anyhow::{anyhow, bail, Result};
use clap::Parser;
use futures::{executor::block_on, future::join_all, StreamExt};
use lapin::options::BasicQosOptions;
use log::{info, error};
use map_rando_videos::{create_object_store, EncodingTask};
use object_store::{path::Path, ObjectStore, PutOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    #[arg(long, env)]
    bunny_url: Option<String>,
    #[arg(long, env)]
    bunny_api_key: Option<String>,
}

struct AppData {
    args: Args,
    db: deadpool_postgres::Pool,
    mq: deadpool_lapin::Pool,
    video_store: Box<dyn ObjectStore>,
    awc: awc::Client,
}

// Using Bunny-specific API to purge the cache is awkward, but currently 
// there aren't other more appealing alternatives.
async fn bunny_purge_file(path: &str, app_data: &AppData) -> Result<()> {
    if app_data.args.bunny_url.is_none() || app_data.args.bunny_api_key.is_none() {
        return Ok(());
    }
    let params = [
        ("async", "false".to_string()),
        ("url", format!("{}/{}", app_data.args.bunny_url.clone().unwrap(), path))
    ];
    let encoded_params = serde_urlencoded::to_string(params)?;
    let req_url = format!("https://api.bunny.net/purge?{encoded_params}");
    info!("Purging Bunny cache: {}", req_url);
    let mut result = 
        app_data.awc.get(req_url)
        .insert_header(("AccessKey", app_data.args.bunny_api_key.clone().unwrap()))
        .send().await.map_err(|e| anyhow!("{:?}", e))?;
    if !result.status().is_success() {
        error!("Response body: {}", String::from_utf8(result.body().await?.to_vec())?);
        bail!("Error purging Bunny cache: {:?}", result);
    }
    Ok(())
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
        awc: awc::Client::default(),
        args,
    };

    Ok(app_data)
}

async fn create_input_pipes(app_data: &AppData, video_id: i32, num_parts: i32) -> Result<()> {
    for part_num in 0..num_parts {
        let object_path = Path::parse(format!("avi-xz/{}-{}.avi.xz", video_id, part_num))?;
        let compressed_input = app_data.video_store.get(&object_path).await?.bytes().await?;
        std::fs::write(format!("/tmp/part-{}.avi.xz", part_num), &compressed_input)?;
        let pipe_path = format!("/tmp/video-{}.pipe", part_num);
        let _ = std::fs::remove_file(&pipe_path);
        unix_named_pipe::create(&pipe_path, Some(0o644))?;
    }

    let mut manifest_file = File::create("/tmp/manifest.txt")?;
    for i in 0..num_parts {
        write!(manifest_file, "file '/tmp/video-{}.pipe'\n", i)?;
    }

    Ok(())
}

async fn feed_part_to_pipe(part_num: i32) -> Result<()> {
    let input_filename = format!("/tmp/part-{}.avi.xz", part_num);
    let compressed_input = tokio::fs::read(input_filename).await.unwrap();
    let mut uncompressed_input =
        async_compression::tokio::bufread::XzDecoder::new(compressed_input.deref());
    let pipe_path = format!("/tmp/video-{}.pipe", part_num);
    let mut pipe = tokio::fs::OpenOptions::new().write(true).open(pipe_path).await?;
    let mut buf = vec![0u8; 65536];
    loop {
        let n = uncompressed_input.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if pipe.write_all(&buf[..n]).await.is_err() {
            // We ignore errors writing to the pipe.
            // A broken pipe is expected since ffmpeg closes the input before reading to the end.
            break;
        }
    }
    Ok(())
}

async fn feed_input_to_pipes(num_parts: i32) -> Result<()> {
    let mut tasks = vec![];
    for part_num in 0..num_parts {
        tasks.push(feed_part_to_pipe(part_num));
    }
    let res = join_all(tasks).await;
    for task_res in res {
        task_res?;
    } 
    Ok(())
}

async fn encode_thumbnail(
    app_data: &AppData,
    video_id: i32,
    num_parts: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    crop_size: i32,
    frame_number: i32,
) -> Result<()> {
    create_input_pipes(app_data, video_id, num_parts).await?;

    let output_path = "/tmp/thumbnail.png";
    let crop_x = crop_center_x - crop_size / 2;
    let crop_y = crop_center_y - crop_size / 2;

    // Run ffmpeg to extract and crop a single selected frame from the video:
    let mut child = Command::new(&app_data.args.ffmpeg_path)
        .arg("-y")
        .arg("-safe")
        .arg("0")
        .arg("-f")
        .arg("concat")
        .arg("-i")
        .arg("/tmp/manifest.txt")
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

    let fut = actix_web::rt::task::spawn_blocking(move || block_on(feed_input_to_pipes(num_parts)));
    let status = child.wait()?;
    info!("ffmpeg {}", status);
    fut.abort();
    if !status.success() {
        bail!("ffmpeg returned non-zero status");
    }

    // Write the output thumbnail to object storage:
    let output_data = std::fs::read(output_path)?;
    let output_key = format!("png/{}.png", video_id);
    let output_path = object_store::path::Path::parse(output_key.clone())?;
    let mut attrs = object_store::Attributes::new();
    attrs.insert(object_store::Attribute::ContentType, "image/png".into());
    let put_opts = PutOptions {
        mode: object_store::PutMode::Overwrite,
        tags: object_store::TagSet::default(),
        attributes: attrs,
    };
    app_data
        .video_store
        .put_opts(&output_path, output_data.into(), put_opts)
        .await?;

    // Invalidate the object cache (Bunny CDN)
    bunny_purge_file(&output_key, app_data).await?;

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
    num_parts: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    crop_size: i32,
    start_frame_number: i32,
    end_frame_number: i32,
) -> Result<()> {
    create_input_pipes(app_data, video_id, num_parts).await?;

    let output_path = "/tmp/highlight.webp";
    let crop_x = crop_center_x - crop_size / 2;
    let crop_y = crop_center_y - crop_size / 2;

    // Run ffmpeg to extract and crop a selected range of frames from the video, cutting the frame rate by a factor of 3:
    let mut child = Command::new(&app_data.args.ffmpeg_path)
        .arg("-y")
        .arg("-safe")
        .arg("0")
        .arg("-f")
        .arg("concat")
        .arg("-i")
        .arg("/tmp/manifest.txt")
        .arg("-vf")
        .arg(&format!(
            "select='between(n\\, {start_frame_number}, {end_frame_number})*not(mod(n-{start_frame_number}\\,3))',crop={crop_size}:{crop_size}:{crop_x}:{crop_y}"
        ))
        .arg("-c:v")
        .arg("libwebp_anim")
        .arg("-lossless")
        .arg("1")
        .arg("-loop")
        .arg("0")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("error spawning ffmpeg");

    let fut = actix_web::rt::task::spawn_blocking(move || block_on(feed_input_to_pipes(num_parts)));
    let status = child.wait()?;
    info!("ffmpeg {}", status);
    fut.abort();
    if !status.success() {
        bail!("ffmpeg returned non-zero status");
    }
    
    // Write the output highlight to object storage:
    let output_data = std::fs::read(output_path)?;
    let output_key = format!("webp/{}.webp", video_id);
    let output_path = object_store::path::Path::parse(output_key.clone())?;
    let mut attrs = object_store::Attributes::new();
    attrs.insert(object_store::Attribute::ContentType, "image/webp".into());
    let put_opts = PutOptions {
        mode: object_store::PutMode::Overwrite,
        tags: object_store::TagSet::default(),
        attributes: attrs,
    };
    app_data
        .video_store
        .put_opts(&output_path, output_data.into(), put_opts)
        .await?;

    // Invalidate the object cache (Bunny CDN)
    bunny_purge_file(&output_key, app_data).await?;

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
    num_parts: i32,
) -> Result<()> {
    create_input_pipes(app_data, video_id, num_parts).await?;

    let output_path = "/tmp/full_video.mp4";

    // Run ffmpeg to encode the video into an mp4. For best compatibility, we use yuv420p pixel format;
    // this subsamples the chroma, which we counteract by upscaling the video resolution by 2x.
    let mut child = Command::new(&app_data.args.ffmpeg_path)
        .arg("-y")
        .arg("-safe")
        .arg("0")
        .arg("-f")
        .arg("concat")
        .arg("-i")
        .arg("/tmp/manifest.txt")
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

    let fut = actix_web::rt::task::spawn_blocking(move || block_on(feed_input_to_pipes(num_parts)));
    let status = child.wait()?;
    info!("ffmpeg {}", status);
    fut.abort();
    if !status.success() {
        bail!("ffmpeg returned non-zero status");
    }
    
    if !status.success() {
        bail!("ffmpeg returned non-zero status");
    }

    // Write the output mp4 to object storage:
    let output_data = std::fs::read(output_path)?;
    let output_key = format!("mp4/{}.mp4", video_id);
    let output_path = object_store::path::Path::parse(output_key.clone())?;
    let mut attrs = object_store::Attributes::new();
    attrs.insert(object_store::Attribute::ContentType, "video/mp4".into());
    let put_opts = PutOptions {
        mode: object_store::PutMode::Overwrite,
        tags: object_store::TagSet::default(),
        attributes: attrs,
    };
    app_data
        .video_store
        .put_opts(&output_path, output_data.into(), put_opts)
        .await?;

    // Invalidate the object cache (Bunny CDN)
    bunny_purge_file(&output_key, app_data).await?;

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
            num_parts,
            crop_center_x,
            crop_center_y,
            crop_size,
            frame_number,
        } => {
            encode_thumbnail(
                &app_data,
                video_id,
                num_parts,
                crop_center_x,
                crop_center_y,
                crop_size,
                frame_number,
            )
            .await?;
        }
        &EncodingTask::HighlightAnimation {
            video_id,
            num_parts,
            crop_center_x,
            crop_center_y,
            crop_size,
            start_frame_number,
            end_frame_number,
        } => {
            encode_highlight(
                app_data,
                video_id,
                num_parts,
                crop_center_x,
                crop_center_y,
                crop_size,
                start_frame_number,
                end_frame_number,
            )
            .await?;
        }
        &EncodingTask::FullVideo { video_id, num_parts } => {
            encode_full_video(app_data, video_id, num_parts).await?;
        }
    }
    Ok(())
}

#[actix_web::main]
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
