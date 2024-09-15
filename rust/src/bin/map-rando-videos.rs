use actix_web::{
    self, delete,
    error::ErrorNotFound,
    get,
    http::StatusCode,
    middleware::{Compress, Logger},
    post, web, App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use actix_web_httpauth::extractors::basic::BasicAuth;
use anyhow::{bail, Context, Result};
use askama::Template;
use clap::Parser;
use core::str;
use futures_util::StreamExt as _;
use log::{error, info};
use map_rando_videos::{create_object_store, EncodingTask};
use object_store::ObjectStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr as _;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::join;
use tokio_postgres::types::ToSql;

#[derive(strum::EnumString, Serialize)]
enum Permission {
    Default,
    Editor,
}

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
    video_storage_prefix: String,
    #[arg(long, env)]
    video_storage_client_url: String,
    #[arg(long, env)]
    xz_compression_level: i32,
}

struct AppData {
    args: Args,
    db: deadpool_postgres::Pool,
    video_store: Box<dyn ObjectStore>,
    mq: deadpool_lapin::Pool,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    video_storage_client_url: String,
    og_title: Option<String>,
    video_id: Option<i32>,
    video_statuses: Vec<String>,
    difficulty_levels: Vec<String>,
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

    // actix_web::rt::spawn
    AppData {
        video_store: create_object_store(&args.video_storage_bucket_url),
        db: db_pool,
        mq: mq_pool,
        args,
    }
}

#[derive(Deserialize)]
struct HomeQuery {
    video_id: Option<i32>,
}

fn get_difficulty_levels() -> Vec<String> {
    vec![
        "Uncategorized",
        "Basic",
        "Medium",
        "Hard",
        "Very Hard",
        "Expert",
        "Extreme",
        "Insane",
        "Beyond",
    ]
    .into_iter()
    .map(|x| x.to_string())
    .collect()
}

#[get("/")]
async fn home(app_data: web::Data<AppData>, query: web::Query<HomeQuery>) -> impl Responder {
    let home_template = HomeTemplate {
        video_storage_client_url: app_data.args.video_storage_client_url.clone(),
        video_id: query.video_id,
        og_title: None,
        video_statuses: vec!["Incomplete", "Complete", "Approved", "Disabled"]
            .into_iter()
            .map(|x| x.to_string())
            .collect(),
        difficulty_levels: get_difficulty_levels(),
    };
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(home_template.render().unwrap())
}

#[get("/video/{video_id}")]
async fn video_html(
    app_data: web::Data<AppData>,
    video_id: web::Path<i32>,
) -> actix_web::Result<impl Responder> {
    let sql = r#"
        SELECT 
            r.name as room_name,
            s.name as strat_name
        FROM video v
        LEFT JOIN room r ON r.room_id = v.room_id
        LEFT JOIN strat s ON s.room_id = v.room_id AND s.strat_id = v.strat_id
        WHERE v.id = $1
    "#;
    let db = app_data.db.get().await.unwrap();
    let stmt = db.prepare_cached(&sql).await.unwrap();
    let row = db
        .query_one(&stmt, &[&*video_id])
        .await
        .map_err(|e| ErrorNotFound(e))?;

    let room_name: Option<String> = row.get("room_name");
    let strat_name: Option<String> = row.get("strat_name");
    let og_title = if room_name.is_some() && strat_name.is_some() {
        Some(format!("{}: {}", room_name.unwrap(), strat_name.unwrap()))
    } else if room_name.is_some() {
        room_name
    } else if strat_name.is_some() {
        strat_name
    } else {
        None
    };

    let home_template = HomeTemplate {
        video_storage_client_url: app_data.args.video_storage_client_url.clone(),
        video_id: Some(*video_id),
        og_title,
        video_statuses: vec!["Incomplete", "Complete", "Approved", "Disabled"]
            .into_iter()
            .map(|x| x.to_string())
            .collect(),
        difficulty_levels: get_difficulty_levels(),
    };
    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(home_template.render().unwrap()))
}

struct AccountInfo {
    id: i32,
    permission: Permission,
}

async fn authenticate(app_data: web::Data<AppData>, auth: &BasicAuth) -> Result<AccountInfo> {
    let db_client = app_data.db.get().await.unwrap();
    let stmt = db_client
        .prepare_cached("SELECT id, token_hash, permission FROM account WHERE username=$1")
        .await
        .context("preparing statement")?;
    let username = auth.user_id().to_owned();
    let result = db_client.query_opt(&stmt, &[&username]).await?;
    match result {
        None => bail!("user not found"),
        Some(row) => {
            let id: i32 = row.get("id");
            let stored_token_hash: Vec<u8> = row.get("token_hash");
            let permission_str: String = row.get("permission");

            let mut hasher = Sha256::new();
            hasher.update(auth.password().unwrap_or(""));
            let presented_token_hash: Vec<u8> = hasher.finalize().to_vec();

            if stored_token_hash == presented_token_hash {
                let permission =
                    Permission::from_str(&permission_str).context("parsing permission")?;
                Ok(AccountInfo { id, permission })
            } else {
                bail!("incorrect token")
            }
        }
    }
}

#[derive(Serialize)]
struct SignInResponse {
    user_id: i32,
    permission: Permission,
}

#[get("/sign-in")]
async fn sign_in(
    app_data: web::Data<AppData>,
    auth: BasicAuth,
) -> actix_web::Result<impl Responder> {
    match authenticate(app_data.clone(), &auth).await {
        Ok(account_info) => {
            let response = SignInResponse {
                user_id: account_info.id,
                permission: account_info.permission,
            };
            Ok(web::Json(response))
        }
        Err(e) => {
            error!("Failed sign-in: {}", e);
            Err(actix_web::error::ErrorUnauthorized(""))
        }
    }
}

async fn try_upload_video(
    req: &HttpRequest,
    mut gzip_payload: web::Payload,
    app_data: web::Data<AppData>,
    account_info: &AccountInfo,
) -> Result<i32> {
    let video_id: Option<i32> = if let Some(h) = req.headers().get("X-MapRandoVideos-VideoId") {
        let s = str::from_utf8(h.as_bytes())?;
        Some(i32::from_str(s)?)
    } else {
        None
    };
    info!("video_id: {:?}", video_id);
    let num_parts = {
        let v = req
            .headers()
            .get("X-MapRandoVideos-NumParts")
            .context("missing X-MapRandoVideos-NumParts")?;
        let s = str::from_utf8(v.as_bytes())?;
        i32::from_str(s)?
    };
    info!("num_parts: {}", num_parts);
    let part_num = {
        let v = req
            .headers()
            .get("X-MapRandoVideos-PartNum")
            .context("missing X-MapRandoVideos-PartNum")?;
        let s = str::from_utf8(v.as_bytes())?;
        i32::from_str(s)?
    };
    info!("part_num: {}", part_num);

    if part_num == 0 && video_id.is_some() {
        bail!("Unexpected X-MapRandoVideos-VideoId header on first part.");
    } else if part_num != 0 && video_id.is_none() {
        bail!("Missing X-MapRandoVideos-VideoId header on non-first part.");
    }

    let db_client = app_data.db.get().await.unwrap();
    let id = if let Some(id) = video_id {
        id
    } else {
        let sql = "SELECT nextval('video_id_seq')";
        let stmt = db_client.prepare_cached(sql).await?;
        let result = db_client.query_one(&stmt, &[]).await?;
        let id = result.get::<_, i64>(0) as i32;
        id
    };

    if part_num != 0 {
        let sql = r#"
            SELECT next_part_num
            FROM video
            WHERE id = $1 AND created_account_id = $2
        "#;
        let stmt = db_client.prepare_cached(sql).await?;
        let result = db_client.query_one(&stmt, &[&id, &account_info.id]).await?;
        let next_part_num: i32 = result.get(0);
        if next_part_num != part_num {
            bail!(
                "Out-of-sequence part number {}. Expecting {}",
                part_num,
                next_part_num
            );
        }
    }

    let mut compressed_data: Vec<u8> = vec![];
    let xz_enc = async_compression::tokio::write::XzEncoder::with_quality(
        &mut compressed_data,
        async_compression::Level::Precise(app_data.args.xz_compression_level),
    );
    let mut gz_dec = async_compression::tokio::write::GzipDecoder::new(xz_enc);

    info!(
        "Compressing video id={} from user_id={}",
        id, account_info.id,
    );

    while let Some(item) = gzip_payload.next().await {
        gz_dec.write(&item?).await?;
    }
    gz_dec.shutdown().await?;
    let mut xz_enc = gz_dec.into_inner();
    xz_enc.shutdown().await?;

    let object_path = object_store::path::Path::parse(format!(
        "{}avi-xz/{}-{}.avi.xz",
        app_data.args.video_storage_prefix, id, part_num
    ))?;
    let compressed_len = compressed_data.len();
    info!(
        "Storing compressed video at {}/{} ({} bytes)",
        app_data.args.video_storage_bucket_url, object_path, compressed_len
    );
    app_data
        .video_store
        .put(&object_path, compressed_data.into())
        .await?;
    info!(
        "Done storing video {}/{} ({} bytes)",
        app_data.args.video_storage_bucket_url, object_path, compressed_len
    );

    if part_num == 0 {
        let sql = r#"
            INSERT INTO video (id, num_parts, next_part_num, status, created_account_id, updated_account_id)
            VALUES ($1, $2, 1, 'Pending', $3, $3)
        "#;
        let stmt = db_client.prepare_cached(sql).await?;
        db_client
            .execute(&stmt, &[&id, &num_parts, &account_info.id])
            .await?;
        info!("Inserted video into database (id={})", id);
    } else {
        let sql = r#"
            UPDATE video 
            SET next_part_num = $1
            WHERE id = $2
        "#;
        let stmt = db_client.prepare_cached(sql).await?;
        let next_part_num = part_num + 1;
        db_client.execute(&stmt, &[&next_part_num, &id]).await?;
        info!("Updated next_part_num (id={})", id);
    }

    Ok(id)
}

#[post("/upload-video")]
async fn upload_video(
    req: HttpRequest,
    payload: web::Payload,
    app_data: web::Data<AppData>,
    auth: BasicAuth,
) -> impl Responder {
    let account_info = match authenticate(app_data.clone(), &auth).await {
        Ok(ai) => ai,
        Err(e) => {
            error!("Failed authentication: {}", e);
            return HttpResponse::Unauthorized().body("Unauthorized");
        }
    };

    let id = match try_upload_video(&req, payload, app_data, &account_info).await {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to upload video: {}", e);
            return HttpResponse::InternalServerError().body("Failed to upload video");
        }
    };

    // TODO: return video ID
    HttpResponse::Ok().body(id.to_string())
}

#[derive(Deserialize, Debug)]
struct SubmitVideoRequest {
    video_id: i32,
    room_id: Option<i32>,
    from_node_id: Option<i32>,
    to_node_id: Option<i32>,
    strat_id: Option<i32>,
    note: String,
    crop_size: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    thumbnail_t: i32,
    highlight_start_t: i32,
    highlight_end_t: i32,
    copyright_waiver: bool,
}

async fn try_submit_video(
    req_json: web::Bytes,
    app_data: web::Data<AppData>,
    account_info: &AccountInfo,
) -> Result<()> {
    info!("submit_video: {}", std::str::from_utf8(&req_json)?);
    let req: SubmitVideoRequest = serde_json::from_slice(&req_json)?;

    if !req.copyright_waiver {
        bail!("copyright_waiver not checked");
    }
    let status: &'static str;
    if req.room_id.is_some()
        && req.from_node_id.is_some()
        && req.to_node_id.is_some()
        && req.strat_id.is_some()
    {
        status = "Complete";
    } else {
        status = "Incomplete";
    }

    let db_client = app_data.db.get().await.unwrap();
    let sql = "SELECT num_parts FROM video WHERE id=$1";
    let stmt = db_client.prepare_cached(&sql).await?;
    let result = db_client.query_one(&stmt, &[&req.video_id]).await?;
    let num_parts: i32 = result.get(0);

    let sql = r#"
        UPDATE video
        SET status = $14,
            updated_ts=current_timestamp,
            submitted_ts=current_timestamp,
            room_id=$2,
            from_node_id=$3,
            to_node_id=$4,
            strat_id=$5,
            note=$6,
            crop_size=$7,
            crop_center_x=$8,
            crop_center_y=$9,
            thumbnail_t=$10,
            highlight_start_t=$11,
            highlight_end_t=$12
        WHERE id=$13 AND created_account_id=$1 AND next_part_num = num_parts
    "#;
    let stmt = db_client.prepare_cached(sql).await?;
    let cnt = db_client
        .execute(
            &stmt,
            &[
                &account_info.id,
                &req.room_id,
                &req.from_node_id,
                &req.to_node_id,
                &req.strat_id,
                &req.note,
                &req.crop_size,
                &req.crop_center_x,
                &req.crop_center_y,
                &req.thumbnail_t,
                &req.highlight_start_t,
                &req.highlight_end_t,
                &req.video_id,
                &status,
            ],
        )
        .await?;
    if cnt == 1 {
        info!("Submitted video: id={}", req.video_id);
    } else {
        bail!(
            "Unexpected update row count: {} (upload may be incomplete?)",
            cnt
        );
    }

    // Send messages to RabbitMQ to trigger processes to encode the thumbnail image, animated highlight, and full video.
    let mq = app_data.mq.get().await?;
    let channel = mq.create_channel().await?;
    let props = lapin::BasicProperties::default().with_delivery_mode(2); // persistent delivery

    let thumbnail_task = EncodingTask::ThumbnailImage {
        video_id: req.video_id,
        num_parts,
        crop_center_x: req.crop_center_x,
        crop_center_y: req.crop_center_y,
        crop_size: req.crop_size,
        frame_number: req.thumbnail_t,
    };
    channel
        .basic_publish(
            "",
            &app_data.args.rabbit_queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&thumbnail_task)?,
            props.clone(),
        )
        .await?;

    let highlight_task = EncodingTask::HighlightAnimation {
        video_id: req.video_id,
        num_parts,
        crop_center_x: req.crop_center_x,
        crop_center_y: req.crop_center_y,
        crop_size: req.crop_size,
        start_frame_number: req.highlight_start_t,
        end_frame_number: req.highlight_end_t,
    };
    channel
        .basic_publish(
            "",
            &app_data.args.rabbit_queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&highlight_task)?,
            props.clone(),
        )
        .await?;

    let full_video_task = EncodingTask::FullVideo {
        video_id: req.video_id,
        num_parts,
    };
    channel
        .basic_publish(
            "",
            &app_data.args.rabbit_queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&full_video_task)?,
            props.clone(),
        )
        .await?;

    // Set the user account to active so it will show in the user listing:
    let sql = "UPDATE account SET active = TRUE WHERE id = $1";
    let stmt = db_client.prepare_cached(sql).await?;
    let cnt = db_client.execute(&stmt, &[&account_info.id]).await?;
    if cnt != 1 {
        bail!("Error updating account 'active'");
    }

    Ok(())
}

#[post("/submit-video")]
async fn submit_video(
    req_json: web::Bytes,
    app_data: web::Data<AppData>,
    auth: BasicAuth,
) -> impl Responder {
    let account_info = match authenticate(app_data.clone(), &auth).await {
        Ok(ai) => ai,
        Err(e) => {
            error!("Failed authentication: {}", e);
            return HttpResponse::Unauthorized().body("Unauthorized");
        }
    };

    match try_submit_video(req_json, app_data.clone(), &account_info).await {
        Ok(_) => {}
        Err(e) => {
            error!("Failed to submit video: {}", e);
            return HttpResponse::InternalServerError().body("Failed to submit video");
        }
    }

    HttpResponse::Ok().body("")
}

#[derive(Deserialize, Debug)]
struct EditVideoRequest {
    video_id: i32,
    room_id: Option<i32>,
    from_node_id: Option<i32>,
    to_node_id: Option<i32>,
    strat_id: Option<i32>,
    note: String,
    crop_size: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    thumbnail_t: i32,
    highlight_start_t: i32,
    highlight_end_t: i32,
    status: VideoStatus,
}

async fn try_edit_video(
    req_json: web::Bytes,
    app_data: web::Data<AppData>,
    account_info: &AccountInfo,
) -> Result<()> {
    info!("edit_video: {}", std::str::from_utf8(&req_json)?);
    let req: EditVideoRequest = serde_json::from_slice(&req_json)?;

    let db_client = app_data.db.get().await.unwrap();
    match account_info.permission {
        Permission::Editor => {
            // Editors are authorized to edit any video, so no check needed.
        }
        Permission::Default => {
            if req.status == VideoStatus::Approved {
                bail!("Not authorized to set this video as Approved");
            }

            let sql = "SELECT updated_account_id FROM video WHERE id=$1";
            let stmt = db_client.prepare_cached(&sql).await?;
            let row = db_client.query_one(&stmt, &[&req.video_id]).await?;
            let updated_account_id: i32 = row.get(0);
            if updated_account_id != account_info.id {
                // It would be more "correct" to return 403 here (and 404 in case the row doesn't exist).
                bail!("Not authorized to edit this video");
            }
        }
    }

    let sql = "SELECT num_parts FROM video WHERE id=$1";
    let stmt = db_client.prepare_cached(&sql).await?;
    let result = db_client.query_one(&stmt, &[&req.video_id]).await?;
    let num_parts: i32 = result.get(0);

    let sql = r#"
        UPDATE video
        SET updated_account_id=$2,
            updated_ts=current_timestamp,
            status=$3,
            room_id=$4,
            from_node_id=$5,
            to_node_id=$6,
            strat_id=$7,
            note=$8,
            crop_size=$9,
            crop_center_x=$10,
            crop_center_y=$11,
            thumbnail_t=$12,
            highlight_start_t=$13,
            highlight_end_t=$14
        WHERE id=$1
    "#;
    let stmt = db_client.prepare_cached(sql).await?;
    let status_str: String = format!("{:?}", req.status);
    let _ = db_client
        .execute(
            &stmt,
            &[
                &req.video_id,
                &account_info.id,
                &status_str,
                &req.room_id,
                &req.from_node_id,
                &req.to_node_id,
                &req.strat_id,
                &req.note,
                &req.crop_size,
                &req.crop_center_x,
                &req.crop_center_y,
                &req.thumbnail_t,
                &req.highlight_start_t,
                &req.highlight_end_t,
            ],
        )
        .await?;
    info!("Edited video");

    // Send messages to RabbitMQ to trigger processes to encode the thumbnail image and animated highlight.
    // The full video cannot change, so no need to encode it again. We could optimize this by checking if
    // thumbnail and/or highlight actually need to be recomputed, but it's cheap so we don't bother for now.
    let mq = app_data.mq.get().await?;
    let channel = mq.create_channel().await?;
    let props = lapin::BasicProperties::default().with_delivery_mode(2); // persistent delivery

    let thumbnail_task = EncodingTask::ThumbnailImage {
        video_id: req.video_id,
        num_parts,
        crop_center_x: req.crop_center_x,
        crop_center_y: req.crop_center_y,
        crop_size: req.crop_size,
        frame_number: req.thumbnail_t,
    };
    channel
        .basic_publish(
            "",
            &app_data.args.rabbit_queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&thumbnail_task)?,
            props.clone(),
        )
        .await?;

    let highlight_task = EncodingTask::HighlightAnimation {
        video_id: req.video_id,
        num_parts,
        crop_center_x: req.crop_center_x,
        crop_center_y: req.crop_center_y,
        crop_size: req.crop_size,
        start_frame_number: req.highlight_start_t,
        end_frame_number: req.highlight_end_t,
    };
    channel
        .basic_publish(
            "",
            &app_data.args.rabbit_queue,
            lapin::options::BasicPublishOptions::default(),
            &serde_json::to_vec(&highlight_task)?,
            props.clone(),
        )
        .await?;
    Ok(())
}

#[post("/edit-video")]
async fn edit_video(
    req_json: web::Bytes,
    app_data: web::Data<AppData>,
    auth: BasicAuth,
) -> impl Responder {
    let account_info = match authenticate(app_data.clone(), &auth).await {
        Ok(ai) => ai,
        Err(e) => {
            error!("Failed authentication: {}", e);
            return HttpResponse::Unauthorized().body("Unauthorized");
        }
    };

    match try_edit_video(req_json, app_data.clone(), &account_info).await {
        Ok(_) => {}
        Err(e) => {
            error!("Failed to edit video: {}", e);
            return HttpResponse::InternalServerError().body("Failed to edit video");
        }
    }

    HttpResponse::Ok().body("")
}

#[derive(Deserialize)]
struct DeleteVideoRequest {
    video_id: i32,
}

#[delete("/")]
async fn delete_video(
    req: web::Query<DeleteVideoRequest>,
    app_data: web::Data<AppData>,
    auth: BasicAuth,
) -> actix_web::Result<impl Responder> {
    let account_info = match authenticate(app_data.clone(), &auth).await {
        Ok(ai) => ai,
        Err(e) => {
            error!("Failed authentication: {}", e);
            return Err(actix_web::error::ErrorUnauthorized("Authentication failed"));
        }
    };

    // Check that the user is authorized to delete this video:
    let db_client = app_data.db.get().await.unwrap();
    let sql = "SELECT updated_account_id, permanent FROM video WHERE id=$1";
    let stmt = db_client.prepare_cached(&sql).await.unwrap();
    let row = db_client
        .query_one(&stmt, &[&req.video_id])
        .await
        .map_err(|e| actix_web::error::ErrorNotFound(e))?;
    let updated_account_id: i32 = row.get(0);
    let permanent: bool = row.get(1);
    if permanent {
        return Err(actix_web::error::ErrorForbidden(
            "video is permanent and may not be deleted",
        ));
    }
    match account_info.permission {
        Permission::Editor => {
            // Editors are authorized to delete any non-permanent video, so no check needed.
        }
        Permission::Default => {
            // Other users are only authorized to delete their own videos.
            if updated_account_id != account_info.id {
                return Err(actix_web::error::ErrorForbidden(
                    "not permitted to delete this video",
                ));
            }
        }
    }

    let sql = "DELETE FROM video WHERE id=$1";
    let stmt = db_client
        .prepare_cached(&sql)
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    let cnt = db_client
        .execute(&stmt, &[&req.video_id])
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    if cnt != 1 {
        return Err(actix_web::error::ErrorInternalServerError(format!(
            "Unexpected deleted row count: {}",
            cnt
        )));
    }

    Ok(HttpResponse::Ok().body(""))
}

#[derive(Deserialize)]
struct DownloadVideoRequest {
    video_id: i32,
    part_num: i32,
}

#[get("/download-video")]
async fn download_video(
    req: web::Query<DownloadVideoRequest>,
    app_data: web::Data<AppData>,
    auth: BasicAuth,
) -> actix_web::Result<impl Responder> {
    let _ = match authenticate(app_data.clone(), &auth).await {
        Ok(ai) => ai,
        Err(e) => {
            error!("Failed authentication: {}", e);
            return Err(actix_web::error::ErrorUnauthorized("Unauthorized"));
        }
    };

    // TODO: Maybe figure out how to make this work by streaming the whole process
    let object_path = format!(
        "{}avi-xz/{}-{}.avi.xz",
        app_data.args.video_storage_prefix, req.video_id, req.part_num
    );
    info!("downloading {}", object_path);
    let xz_data = app_data
        .video_store
        .get(
            &object_store::path::Path::parse(&object_path)
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?,
        )
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?
        .bytes()
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    info!("decompressing & recompressing {}", object_path);
    let uncompressed = async_compression::tokio::bufread::XzDecoder::new(&*xz_data);
    let buf_uncompressed = tokio::io::BufReader::new(uncompressed);
    let mut gz_enc = async_compression::tokio::bufread::GzipEncoder::new(buf_uncompressed);
    let mut output: Vec<u8> = vec![];
    gz_enc.read_to_end(&mut output).await?;
    info!("responding with recompressed {}", object_path);
    Ok(HttpResponse::Ok().body(output))
}

#[derive(Serialize)]
struct UserListing {
    id: i32,
    username: String,
}

async fn try_list_users(app_data: web::Data<AppData>) -> Result<Vec<UserListing>> {
    let db_client = app_data.db.get().await.unwrap();
    let sql = "SELECT id, username FROM account WHERE active";
    let stmt = db_client.prepare_cached(sql).await?;
    let result = db_client.query(&stmt, &[]).await?;
    let mut out = vec![];
    for row in result {
        out.push(UserListing {
            id: row.get(0),
            username: row.get(1),
        })
    }
    Ok(out)
}

#[get("/list-users")]
async fn list_users(app_data: web::Data<AppData>) -> actix_web::Result<impl Responder> {
    let v = try_list_users(app_data)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(v))
}

#[derive(Serialize, Deserialize, strum::EnumString)]
enum ListVideosSortBy {
    SubmittedTimestamp,
    UpdatedTimestamp,
}

#[derive(Deserialize)]
struct ListVideosRequest {
    room_id: Option<i32>,
    from_node_id: Option<i32>,
    to_node_id: Option<i32>,
    strat_id: Option<i32>,
    user_id: Option<i32>,
    video_id: Option<i32>,
    status_list: String,
    sort_by: ListVideosSortBy,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Serialize, Deserialize, strum::EnumString, Debug, Eq, PartialEq)]
enum VideoStatus {
    Pending,
    Incomplete,
    Complete,
    Approved,
    Disabled,
}

#[derive(Serialize)]
struct VideoListing {
    id: i32,
    created_user_id: i32,
    submitted_ts: i64,
    updated_user_id: i32,
    updated_ts: i64,
    room_id: Option<i32>,
    from_node_id: Option<i32>,
    to_node_id: Option<i32>,
    strat_id: Option<i32>,
    note: String,
    status: VideoStatus,
    room_name: Option<String>,
    from_node_name: Option<String>,
    to_node_name: Option<String>,
    strat_name: Option<String>,
}

async fn try_list_videos(req: &ListVideosRequest, app_data: &AppData) -> Result<Vec<VideoListing>> {
    let mut sql_parts: Vec<String> = vec![];
    sql_parts.push(format!(
        r#"
        SELECT 
            v.id,
            v.created_account_id,
            v.submitted_ts,
            v.updated_account_id,
            v.updated_ts,
            v.room_id,
            v.from_node_id,
            v.to_node_id,
            v.strat_id,
            v.note,
            v.status,
            r.name as room_name,
            f.name as from_node_name,
            t.name as to_node_name,
            s.name as strat_name
        FROM video v
        LEFT JOIN room r ON r.room_id = v.room_id
        LEFT JOIN node f ON f.room_id = v.room_id AND f.node_id = v.from_node_id
        LEFT JOIN node t ON t.room_id = v.room_id AND t.node_id = v.to_node_id
        LEFT JOIN strat s ON s.room_id = v.room_id AND s.strat_id = v.strat_id
        "#
    ));

    let mut sql_filters: Vec<String> = vec![];
    let mut param_values: Vec<&(dyn ToSql + Sync)> = vec![];

    sql_filters.push("submitted_ts IS NOT NULL".to_string());
    if req.room_id.is_some() {
        sql_filters.push(format!("v.room_id = ${}", param_values.len() + 1));
        param_values.push(req.room_id.as_ref().unwrap());
    }
    if req.from_node_id.is_some() {
        sql_filters.push(format!("v.from_node_id = ${}", param_values.len() + 1));
        param_values.push(req.from_node_id.as_ref().unwrap());
    }
    if req.to_node_id.is_some() {
        sql_filters.push(format!("v.to_node_id = ${}", param_values.len() + 1));
        param_values.push(req.to_node_id.as_ref().unwrap());
    }
    if req.strat_id.is_some() {
        sql_filters.push(format!("v.strat_id = ${}", param_values.len() + 1));
        param_values.push(req.strat_id.as_ref().unwrap());
    }
    if req.video_id.is_some() {
        sql_filters.push(format!("v.id = ${}", param_values.len() + 1));
        param_values.push(req.video_id.as_ref().unwrap());
    }
    if req.user_id.is_some() {
        sql_filters.push(format!(
            "v.created_account_id = ${}",
            param_values.len() + 1
        ));
        param_values.push(req.user_id.as_ref().unwrap());
    }
    sql_filters.push(format!(
        "v.status = ANY(regexp_split_to_array(${},','))",
        param_values.len() + 1
    ));
    param_values.push(&req.status_list);
    if sql_filters.len() > 0 {
        sql_parts.push(format!("WHERE {}\n", sql_filters.join(" AND ")));
    }

    match req.sort_by {
        ListVideosSortBy::SubmittedTimestamp => {
            sql_parts.push("ORDER BY v.submitted_ts DESC\n".to_string());
        }
        ListVideosSortBy::UpdatedTimestamp => {
            sql_parts.push("ORDER BY v.updated_ts DESC\n".to_string());
        }
    }

    if req.limit.is_some() {
        sql_parts.push(format!("LIMIT ${}\n", param_values.len() + 1));
        param_values.push(req.limit.as_ref().unwrap());
    }
    if req.offset.is_some() {
        sql_parts.push(format!("OFFSET ${}\n", param_values.len() + 1));
        param_values.push(req.offset.as_ref().unwrap());
    }

    let sql = sql_parts.join("");
    let db_client = app_data.db.get().await?;

    let stmt = db_client.prepare_cached(&sql).await?;
    let result = db_client.query(&stmt, param_values.as_slice()).await?;
    let mut out: Vec<VideoListing> = vec![];
    for row in result {
        let submitted_ts: chrono::DateTime<chrono::offset::Utc> = row.get("submitted_ts");
        let updated_ts: chrono::DateTime<chrono::offset::Utc> = row.get("updated_ts");
        let status_str: String = row.get("status");
        out.push(VideoListing {
            id: row.get("id"),
            created_user_id: row.get("created_account_id"),
            submitted_ts: submitted_ts.timestamp_millis(),
            updated_user_id: row.get("updated_account_id"),
            updated_ts: updated_ts.timestamp_millis(),
            room_id: row.get("room_id"),
            from_node_id: row.get("from_node_id"),
            to_node_id: row.get("to_node_id"),
            strat_id: row.get("strat_id"),
            note: row.get("note"),
            status: VideoStatus::try_from(status_str.as_str())?,
            room_name: row.get("room_name"),
            from_node_name: row.get("from_node_name"),
            to_node_name: row.get("to_node_name"),
            strat_name: row.get("strat_name"),
        });
    }

    Ok(out)
}

#[get("/list-videos")]
async fn list_videos(
    req: web::Query<ListVideosRequest>,
    app_data: web::Data<AppData>,
) -> actix_web::Result<impl Responder> {
    let out = try_list_videos(&req, &app_data)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(out))
}

#[derive(Deserialize)]
struct GetVideoRequest {
    video_id: i32,
}

#[derive(Serialize)]
struct GetVideoResponse {
    num_parts: i32,
    room_id: Option<i32>,
    from_node_id: Option<i32>,
    to_node_id: Option<i32>,
    strat_id: Option<i32>,
    note: String,
    crop_size: i32,
    crop_center_x: i32,
    crop_center_y: i32,
    thumbnail_t: i32,
    highlight_start_t: i32,
    highlight_end_t: i32,
    status: VideoStatus,
    permanent: bool,
}

#[get("/get-video")]
async fn get_video(
    req: web::Query<GetVideoRequest>,
    app_data: web::Data<AppData>,
) -> actix_web::Result<impl Responder> {
    let sql = r#"
        SELECT 
            num_parts,
            room_id,
            from_node_id,
            to_node_id,
            strat_id,
            note,
            crop_size,
            crop_center_x,
            crop_center_y,
            thumbnail_t,
            highlight_start_t,
            highlight_end_t,
            status,
            permanent
        FROM video
        WHERE id = $1
    "#;
    let db =
        app_data.db.get().await.map_err(|e| {
            actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR)
        })?;
    let stmt = db
        .prepare_cached(&sql)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    let row = db
        .query_one(&stmt, &[&req.video_id])
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::NOT_FOUND))?;

    let status_str: String = row.get("status");
    let response = GetVideoResponse {
        num_parts: row.get("num_parts"),
        room_id: row.get("room_id"),
        from_node_id: row.get("from_node_id"),
        to_node_id: row.get("to_node_id"),
        strat_id: row.get("strat_id"),
        note: row.get("note"),
        status: VideoStatus::try_from(status_str.as_str()).unwrap(),
        crop_size: row.get("crop_size"),
        crop_center_x: row.get("crop_center_x"),
        crop_center_y: row.get("crop_center_y"),
        thumbnail_t: row.get("thumbnail_t"),
        highlight_start_t: row.get("highlight_start_t"),
        highlight_end_t: row.get("highlight_end_t"),
        permanent: row.get("permanent"),
    };
    Ok(web::Json(response))
}

#[derive(Serialize)]
struct AreaOveriew {
    areas: Vec<AreaListing>,
}

#[derive(Serialize)]
struct AreaListing {
    name: String,
    rooms: Vec<RoomListing>,
}

#[derive(Serialize)]
struct RoomListing {
    id: i32,
    name: String,
}

async fn try_list_rooms_by_area(app_data: &AppData) -> Result<AreaOveriew> {
    let db = app_data.db.get().await?;
    let stmt = db
        .prepare_cached("SELECT area_id, name FROM area ORDER BY area_id")
        .await?;
    let area_fut = db.query(&stmt, &[]);

    let stmt = db
        .prepare_cached("SELECT room_id, area_id, name FROM room")
        .await?;
    let room_fut = db.query(&stmt, &[]);
    let (area_result, room_result) = join!(area_fut, room_fut);

    let mut areas: Vec<AreaListing> = vec![];
    for row in area_result? {
        let area_id: i32 = row.get(0);
        if area_id as usize != areas.len() {
            bail!("Unexpected sequence of area IDs");
        }
        let name: String = row.get(1);
        areas.push(AreaListing {
            name,
            rooms: vec![],
        });
    }
    for row in room_result? {
        let room_id: i32 = row.get(0);
        let area_id: i32 = row.get(1);
        let name: String = row.get(2);
        areas[area_id as usize]
            .rooms
            .push(RoomListing { id: room_id, name });
    }

    Ok(AreaOveriew { areas })
}

#[get("/rooms-by-area")]
async fn list_rooms_by_area(app_data: web::Data<AppData>) -> actix_web::Result<impl Responder> {
    let v = try_list_rooms_by_area(&app_data)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(v))
}

#[derive(Deserialize)]
struct NodeListQuery {
    room_id: i32,
}

#[derive(Serialize)]
struct NodeListing {
    id: i32,
    name: String,
}

async fn try_list_nodes(app_data: &AppData, query: &NodeListQuery) -> Result<Vec<NodeListing>> {
    let db = app_data.db.get().await?;
    let stmt = db
        .prepare_cached("SELECT node_id, name FROM node WHERE room_id=$1 ORDER BY node_id")
        .await?;
    let node_rows = db.query(&stmt, &[&query.room_id]).await?;
    let mut node_listings: Vec<NodeListing> = vec![];
    for row in node_rows {
        let node_id: i32 = row.get(0);
        let name: String = row.get(1);
        node_listings.push(NodeListing { id: node_id, name });
    }
    Ok(node_listings)
}

#[get("/nodes")]
async fn list_nodes(
    app_data: web::Data<AppData>,
    query: web::Query<NodeListQuery>,
) -> actix_web::Result<impl Responder> {
    let v = try_list_nodes(&app_data, &query)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(v))
}

#[derive(Deserialize)]
struct StratListQuery {
    room_id: i32,
    from_node_id: i32,
    to_node_id: i32,
}

#[derive(Serialize)]
struct StratListing {
    id: i32,
    name: String,
}

async fn try_list_strats(app_data: &AppData, query: &StratListQuery) -> Result<Vec<StratListing>> {
    let db = app_data.db.get().await?;
    let stmt = db
        .prepare_cached(
            r#"
      SELECT strat_id, name FROM strat 
      WHERE room_id=$1 AND from_node_id=$2 AND to_node_id=$3
      ORDER BY strat_id"#,
        )
        .await?;
    let strat_rows = db
        .query(
            &stmt,
            &[&query.room_id, &query.from_node_id, &query.to_node_id],
        )
        .await?;
    let mut strat_listings: Vec<StratListing> = vec![];
    for row in strat_rows {
        let strat_id: i32 = row.get(0);
        let name: String = row.get(1);
        strat_listings.push(StratListing { id: strat_id, name });
    }
    Ok(strat_listings)
}

#[get("/strats")]
async fn list_strats(
    app_data: web::Data<AppData>,
    query: web::Query<StratListQuery>,
) -> actix_web::Result<impl Responder> {
    let v = try_list_strats(&app_data, &query)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(v))
}

#[derive(Serialize)]
struct TechListing {
    tech_id: i32,
    name: String,
    difficulty: String,
    video_id: Option<i32>,
}

async fn try_list_tech(app_data: &AppData) -> Result<Vec<TechListing>> {
    let db = app_data.db.get().await?;
    let stmt = db
        .prepare_cached(
            r#"
      SELECT 
        t.tech_id,
        t.name,
        s.difficulty,
        s.video_id
      FROM tech t
      LEFT JOIN tech_setting s ON s.tech_id = t.tech_id
      ORDER BY tech_id"#,
        )
        .await?;
    let tech_rows = db.query(&stmt, &[]).await?;
    let mut tech_listings: Vec<TechListing> = vec![];
    for row in tech_rows {
        let tech_id: i32 = row.get("tech_id");
        let name: String = row.get("name");
        let difficulty: Option<String> = row.get("difficulty");
        let video_id: Option<i32> = row.get("video_id");
        tech_listings.push(TechListing {
            tech_id,
            name,
            difficulty: difficulty.unwrap_or("Uncategorized".to_string()),
            video_id,
        });
    }
    Ok(tech_listings)
}

#[get("/tech")]
async fn list_tech(app_data: web::Data<AppData>) -> actix_web::Result<impl Responder> {
    let v = try_list_tech(&app_data)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(v))
}

#[derive(Deserialize, Debug)]
struct TechUpdate {
    tech_id: i32,
    difficulty: String,
    video_id: Option<i32>,
}

async fn try_update_tech(app_data: &AppData, tech_update: &TechUpdate) -> Result<()> {
    let db = app_data.db.get().await?;
    let stmt = db
        .prepare_cached(
            r#"
            INSERT INTO tech_setting (tech_id, difficulty, video_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (tech_id) DO UPDATE SET
                difficulty = $2,
                video_id = $3
        "#,
        )
        .await?;
    let cnt_updated = db
        .execute(
            &stmt,
            &[
                &tech_update.tech_id,
                &tech_update.difficulty,
                &tech_update.video_id,
            ],
        )
        .await?;
    if cnt_updated != 1 {
        error!(
            "Unexpected tech updated count {}: {:?}",
            cnt_updated, tech_update
        );
    }
    Ok(())
}

#[post("/tech")]
async fn update_tech(
    app_data: web::Data<AppData>,
    tech_updates: web::Json<Vec<TechUpdate>>,
    auth: BasicAuth,
) -> impl Responder {
    let account_info = match authenticate(app_data.clone(), &auth).await {
        Ok(ai) => ai,
        Err(e) => {
            error!("Failed authentication: {}", e);
            return HttpResponse::Unauthorized().body("Failed authentication");
        }
    };

    match account_info.permission {
        Permission::Editor => {},
        Permission::Default => {
            return HttpResponse::Unauthorized().body("Unauthorized");
        }
    }

    for tech in &tech_updates.0 {
        if let Err(e) = try_update_tech(&app_data, tech).await {
            return HttpResponse::InternalServerError().body(e.to_string());
        }
    }
    HttpResponse::Ok().body("")
}

#[actix_web::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let app_data = actix_web::web::Data::new(build_app_data().await);

    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .app_data(awc::Client::default())
            .wrap(Compress::default())
            .wrap(Logger::default())
            .service(home)
            .service(video_html)
            .service(sign_in)
            .service(upload_video)
            .service(submit_video)
            .service(list_users)
            .service(list_videos)
            .service(list_rooms_by_area)
            .service(list_nodes)
            .service(list_strats)
            .service(list_tech)
            .service(update_tech)
            .service(get_video)
            .service(edit_video)
            .service(delete_video)
            .service(download_video)
            .service(actix_files::Files::new("/js", "../js"))
            .service(actix_files::Files::new("/static", "../static"))
    })
    .bind("0.0.0.0:8081")
    .unwrap()
    .run()
    .await
    .unwrap();
}
