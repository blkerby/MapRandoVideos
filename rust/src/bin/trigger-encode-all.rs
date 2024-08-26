use clap::Parser;
use lapin::{options::QueuePurgeOptions, Channel};
use log::info;
use anyhow::Result;
use map_rando_videos::EncodingTask;

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
    purge_queue: bool,
}

struct AppData {
    args: Args,
    db: deadpool_postgres::Pool,
    mq: deadpool_lapin::Pool,
}

struct VideoData {
    video_id: i32,
    num_parts: i32,
    crop_size: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    thumbnail_t: i32,
    highlight_start_t: i32,
    highlight_end_t: i32,
}

async fn enqueue_video_tasks(channel: &Channel, queue: &str, video: &VideoData) -> Result<()> {
    info!("Processing video {}", video.video_id);
    let props = lapin::BasicProperties::default().with_delivery_mode(2); // persistent delivery

    let thumbnail_task = EncodingTask::ThumbnailImage {
        video_id: video.video_id,
        num_parts: video.num_parts,
        crop_center_x: video.crop_center_x,
        crop_center_y: video.crop_center_y,
        crop_size: video.crop_size,
        frame_number: video.thumbnail_t,
    };
    channel
        .basic_publish(
            "",
            queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&thumbnail_task)?,
            props.clone(),
        )
        .await?;

    let highlight_task = EncodingTask::HighlightAnimation {
        video_id: video.video_id,
        num_parts: video.num_parts,
        crop_center_x: video.crop_center_x,
        crop_center_y: video.crop_center_y,
        crop_size: video.crop_size,
        start_frame_number: video.highlight_start_t,
        end_frame_number: video.highlight_end_t,
    };
    channel
        .basic_publish(
            "",
            queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&highlight_task)?,
            props.clone(),
        )
        .await?;

    let full_video_task = EncodingTask::FullVideo {
        video_id: video.video_id,
        num_parts: video.num_parts,
    };
    channel
        .basic_publish(
            "",
            queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&full_video_task)?,
            props.clone(),
        )
        .await?;

    Ok(())
}

async fn build_app_data() -> AppData {
    let args = Args::parse();

    // Create Postgres connection pool
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

    // Get a test connection, to fail now in case we can't connect to the database.
    let _ = db_pool.get().await.unwrap();

    // Create RabbitMQ connection pool
    let mut cfg = deadpool_lapin::Config::default();
    cfg.url = Some(args.rabbit_url.clone());
    let mq_pool = cfg
        .create_pool(Some(deadpool_lapin::Runtime::Tokio1))
        .unwrap();
    let mq = mq_pool.get().await.unwrap();
    let channel = mq.create_channel().await.unwrap();
    let mut opts = lapin::options::QueueDeclareOptions::default();
    opts.durable = true;
    channel
        .queue_declare(
            &args.rabbit_queue,
            opts,
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();
    if args.purge_queue {
        channel.queue_purge(&args.rabbit_queue, QueuePurgeOptions::default()).await.unwrap();
    }

    AppData {
        db: db_pool,
        mq: mq_pool,
        args,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let app_data = build_app_data().await;

    let db = app_data.db.get().await?;
    let sql = r#"
        SELECT
            id AS video_id,
            num_parts,
            crop_size,
            crop_center_x,
            crop_center_y,
            thumbnail_t,
            highlight_start_t,
            highlight_end_t
        FROM video
        WHERE crop_size IS NOT NULL
        ORDER BY video_id
       "#;
    let stmt = db.prepare(&sql).await?;
    let row_vec = db.query(&stmt, &[]).await?;
    info!("Retrieved metadata for {} videos", row_vec.len());
 
    let mq = app_data.mq.get().await?;
    let channel = mq.create_channel().await?;
    channel.tx_select().await?;
    for row in row_vec {
        let video = VideoData {
            video_id: row.get("video_id"),
            num_parts: row.get("num_parts"),
            crop_size: row.get("crop_size"),
            crop_center_x: row.get("crop_center_x"),
            crop_center_y: row.get("crop_center_y"),
            thumbnail_t: row.get("thumbnail_t"),
            highlight_start_t: row.get("highlight_start_t"),
            highlight_end_t:row.get("highlight_end_t"),
        };
        enqueue_video_tasks(&channel, &app_data.args.rabbit_queue, &video).await?;
    }
    channel.tx_commit().await?;
    info!("Successfully published messages to queue '{}' at {}", app_data.args.rabbit_queue, app_data.args.rabbit_url);
    Ok(())
}