use actix_web::{
    self, get,
    http::StatusCode,
    middleware::{Compress, Logger},
    post, web, App, HttpResponse, HttpServer, Responder,
};
use actix_web_httpauth::extractors::basic::BasicAuth;
use anyhow::{bail, Context, Result};
use askama::Template;
use clap::Parser;
use futures_util::StreamExt as _;
use log::{error, info};
use object_store::{
    gcp::GoogleCloudStorageBuilder, local::LocalFileSystem, memory::InMemory, ObjectStore,
};
use serde::{Deserialize, Serialize};
use std::{path::Path, str::FromStr};
use tokio::io::AsyncWriteExt as _;
use tokio_postgres::types::ToSql;
use tokio::join;

#[derive(strum::EnumString)]
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
    sm_json_data_summary_url: String,
    #[arg(long, env)]
    video_storage_bucket_url: String,
    #[arg(long, env)]
    video_storage_prefix: String,
    #[arg(long, env)]
    xz_compression_level: i32,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    sm_json_data_summary_url: String,
}

struct AppData {
    args: Args,
    db: deadpool_postgres::Pool,
    video_store: Box<dyn ObjectStore>,
}

pub fn create_object_store(url: &str) -> Box<dyn ObjectStore> {
    let object_store: Box<dyn ObjectStore> = if url.starts_with("gs:") {
        Box::new(
            GoogleCloudStorageBuilder::from_env()
                .with_url(url)
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

async fn build_app_data() -> AppData {
    let args = Args::parse();

    let mut config = deadpool_postgres::Config::new();
    config.host = Some(args.postgres_host.clone());
    config.dbname = Some(args.postgres_db.clone());
    config.user = Some(args.postgres_user.clone());
    config.password = Some(args.postgres_password.clone());
    let pool = config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();

    // Get a test connection, to fail now in case we can't connect to the database.
    let _ = pool.get().await.unwrap();

    // actix_web::rt::spawn
    AppData {
        video_store: create_object_store(&args.video_storage_bucket_url),
        db: pool,
        args,
    }
}

#[get("/")]
async fn home(app_data: web::Data<AppData>) -> impl Responder {
    let home_template = HomeTemplate {
        sm_json_data_summary_url: app_data.args.sm_json_data_summary_url.clone(),
    };
    HttpResponse::Ok().body(home_template.render().unwrap())
}

struct AccountInfo {
    id: i32,
    permission: Permission,
}

async fn authenticate(app_data: web::Data<AppData>, auth: &BasicAuth) -> Result<AccountInfo> {
    let db_client = app_data.db.get().await.unwrap();
    let stmt = db_client
        .prepare_cached("SELECT id, token, permission FROM account WHERE username=$1")
        .await
        .context("preparing statement")?;
    let username = auth.user_id().to_owned();
    let result = db_client.query_opt(&stmt, &[&username]).await?;
    match result {
        None => bail!("user not found"),
        Some(row) => {
            let id: i32 = row.get("id");
            let token: String = row.get("token");
            let permission_str: String = row.get("permission");
            if token == auth.password().unwrap_or("") {
                let permission =
                    Permission::from_str(&permission_str).context("parsing permission")?;
                Ok(AccountInfo { id, permission })
            } else {
                bail!("incorrect token")
            }
        }
    }
}

#[get("/sign-in")]
async fn sign_in(app_data: web::Data<AppData>, auth: BasicAuth) -> impl Responder {
    match authenticate(app_data.clone(), &auth).await {
        Ok(_) => HttpResponse::Ok().body(""),
        Err(e) => {
            error!("Failed sign-in: {}", e);
            HttpResponse::Unauthorized().body("")
        }
    }
}

async fn try_upload_video(
    mut gzip_payload: web::Payload,
    app_data: web::Data<AppData>,
    account_info: &AccountInfo,
) -> Result<i32> {
    let sql = "SELECT nextval('video_id_seq')";
    let db_client = app_data.db.get().await.unwrap();
    let stmt = db_client.prepare_cached(sql).await?;
    let result = db_client.query_one(&stmt, &[]).await?;
    let id = result.get::<_, i64>(0) as i32;

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

    let object_path = object_store::path::Path::parse(format!(
        "{}avi-xz/{}.avi.xz",
        app_data.args.video_storage_prefix, id
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

    let sql = r#"
        INSERT INTO video (id, status, created_account_id, updated_account_id)
        VALUES ($1, 'Pending', $2, $2)
    "#;
    let stmt = db_client.prepare_cached(sql).await?;
    db_client.execute(&stmt, &[&id, &account_info.id]).await?;
    info!("Inserted video into database (id={})", id);

    Ok(id)
}

#[post("/upload-video")]
async fn upload_video(
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

    let id = match try_upload_video(payload, app_data, &account_info).await {
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
    let db_client = app_data.db.get().await.unwrap();
    let sql = r#"
        UPDATE video
        SET status = 'New',
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
        WHERE id=$13 AND created_account_id=$1
    "#;
    let stmt = db_client.prepare_cached(sql).await?;
    let _ = db_client
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
            ],
        )
        .await?;
    info!("Submitted video");
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

#[derive(Serialize)]
struct UserListing {
    id: i32,
    username: String,
}

async fn try_list_users(app_data: web::Data<AppData>) -> Result<Vec<UserListing>> {
    let db_client = app_data.db.get().await.unwrap();
    let sql = "SELECT id, username FROM account";
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
    CreatedTimestamp,
    UpdatedTimestamp,
}

#[derive(Deserialize)]
struct ListVideosRequest {
    room_id: Option<i32>,
    from_node_id: Option<i32>,
    to_node_id: Option<i32>,
    strat_id: Option<i32>,
    user_id: Option<i32>,
    sort_by: ListVideosSortBy,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Serialize, Deserialize, strum::EnumString)]
enum VideoStatus {
    Pending,
    New,
    Approved,
    Supplemental,
    Incomplete,
    Obsolete
}

#[derive(Serialize)]
struct VideoListing {
    id: i32,
    created_user_id: i32,
    created_ts: i64,
    updated_user_id: i32,
    updated_ts: i64,
    room_id: Option<i32>,
    from_node_id: Option<i32>,
    to_node_id: Option<i32>,
    strat_id: Option<i32>,
    note: String,
    status: VideoStatus,
}

async fn try_list_videos(req: &ListVideosRequest, app_data: &AppData) -> Result<Vec<VideoListing>> {
    let mut sql_parts = vec![];
    sql_parts.push(format!(
        r#"
        SELECT 
            id,
            created_account_id,
            created_ts,
            updated_account_id,
            updated_ts,
            room_id,
            from_node_id,
            to_node_id,
            strat_id,
            note,
            status
        FROM video
        "#
    ));

    let mut sql_filters = vec![];
    let mut param_values: Vec<&(dyn ToSql + Sync)> = vec![];
    if req.room_id.is_some() {
        sql_filters.push(format!("room_id = ${}", param_values.len() + 1));
        param_values.push(req.room_id.as_ref().unwrap());
    }
    if req.from_node_id.is_some() {
        sql_filters.push(format!("from_node_id = ${}", param_values.len() + 1));
        param_values.push(req.from_node_id.as_ref().unwrap());
    }
    if req.to_node_id.is_some() {
        sql_filters.push(format!("to_node_id = ${}", param_values.len() + 1));
        param_values.push(req.to_node_id.as_ref().unwrap());
    }
    if req.strat_id.is_some() {
        sql_filters.push(format!("strat_id = ${}", param_values.len() + 1));
        param_values.push(req.strat_id.as_ref().unwrap());
    }
    if req.user_id.is_some() {
        sql_filters.push(format!("(created_account_id = ${} OR updated_account_id = ${})", param_values.len() + 1, param_values.len() + 1));
        param_values.push(req.user_id.as_ref().unwrap());
    }
    if sql_filters.len() > 0 {
        sql_parts.push(format!("WHERE {}\n", sql_filters.join(" AND ")));
    }

    match req.sort_by {
        ListVideosSortBy::CreatedTimestamp => {
            sql_parts.push("ORDER BY created_ts DESC\n".to_string());
        }
        ListVideosSortBy::UpdatedTimestamp => {
            sql_parts.push("ORDER BY updated_ts DESC\n".to_string());
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
        let created_ts: chrono::DateTime<chrono::offset::Utc> = row.get("created_ts");
        let updated_ts: chrono::DateTime<chrono::offset::Utc> = row.get("updated_ts");
        let status_str: String = row.get("status");
        out.push(VideoListing {
            id: row.get("id"),
            created_user_id: row.get("created_account_id"),
            created_ts: created_ts.timestamp_millis(),
            updated_user_id: row.get("updated_account_id"),
            updated_ts: updated_ts.timestamp_millis(),
            room_id: row.get("room_id"),
            from_node_id: row.get("from_node_id"),
            to_node_id: row.get("to_node_id"),
            strat_id: row.get("strat_id"),
            note: row.get("note"),
            status: VideoStatus::try_from(status_str.as_str())?,
        });
    }

    Ok(out)
}

#[get("/list-videos")]
async fn list_videos(req: web::Query<ListVideosRequest>, app_data: web::Data<AppData>) -> actix_web::Result<impl Responder> {
    let out = try_list_videos(&req, &app_data)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(out))
}

#[derive(Serialize)]
struct AreaOveriew {
    areas: Vec<AreaListing>
}

#[derive(Serialize)]
struct AreaListing {
    name: String,
    rooms: Vec<RoomListing>,
}

#[derive(Serialize)]
struct RoomListing {
    id: i32,
    name: String
}

async fn try_list_rooms_by_area(app_data: &AppData) -> Result<AreaOveriew> {
    let db = app_data.db.get().await?;
    let stmt = db.prepare_cached("SELECT area_id, name FROM area ORDER BY area_id").await?;
    let area_fut = db.query(&stmt, &[]);

    let stmt = db.prepare_cached("SELECT room_id, area_id, name FROM room").await?;
    let room_fut = db.query(&stmt, &[]);
    let (area_result, room_result) = join!(area_fut, room_fut);
    
    let mut areas: Vec<AreaListing> = vec![];
    for row in area_result? {
        let area_id: i32 = row.get(0);
        if area_id as usize != areas.len() {
            bail!("Unexpected sequence of area IDs");
        }
        let name: String = row.get(1);
        areas.push(AreaListing { name, rooms: vec![] });
    }
    for row in room_result? {
        let room_id: i32 = row.get(0);
        let area_id: i32 = row.get(1);
        let name: String = row.get(2);
        areas[area_id as usize].rooms.push(RoomListing {
            id: room_id,
            name
        });
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
    let stmt = db.prepare_cached("SELECT node_id, name FROM node WHERE room_id=$1 ORDER BY node_id").await?;
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
async fn list_nodes(app_data: web::Data<AppData>, query: web::Query<NodeListQuery>) -> actix_web::Result<impl Responder> {
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
    let stmt = db.prepare_cached(r#"
      SELECT strat_id, name FROM strat 
      WHERE room_id=$1 AND from_node_id=$2 AND to_node_id=$3
      ORDER BY strat_id"#).await?;
    let strat_rows = db.query(&stmt, &[&query.room_id, &query.from_node_id, &query.to_node_id]).await?;
    let mut strat_listings: Vec<StratListing> = vec![];
    for row in strat_rows {
        let strat_id: i32 = row.get(0);
        let name: String = row.get(1);
        strat_listings.push(StratListing { id: strat_id, name });
    }    
    Ok(strat_listings)
}

#[get("/strats")]
async fn list_strats(app_data: web::Data<AppData>, query: web::Query<StratListQuery>) -> actix_web::Result<impl Responder> {
    let v = try_list_strats(&app_data, &query)
        .await
        .map_err(|e| actix_web::error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(web::Json(v))
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
            .wrap(Compress::default())
            .wrap(Logger::default())
            .service(home)
            .service(sign_in)
            .service(upload_video)
            .service(submit_video)
            .service(list_users)
            .service(list_videos)
            .service(list_rooms_by_area)
            .service(list_nodes)
            .service(list_strats)
            .service(actix_files::Files::new("/js", "../js"))
            .service(actix_files::Files::new("/static", "static"))
    })
    .bind("0.0.0.0:8081")
    .unwrap()
    .run()
    .await
    .unwrap();
}
