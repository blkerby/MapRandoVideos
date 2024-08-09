use actix_web::{
    self, get,
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
use serde::Deserialize;
use std::{path::Path, str::FromStr};
use tokio::io::AsyncWriteExt as _;

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

    let sql = "INSERT INTO video (id, status, created_account_id) VALUES ($1, 'Pending', $2)";
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
        INSERT INTO video (
            status,
            created_account_id,
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
            highlight_end_t
        ) 
        VALUES ('Pending', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
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
            ],
        )
        .await?;
    info!("Inserted video");
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
            .service(actix_files::Files::new("/js", "../js"))
    })
    .bind("0.0.0.0:8081")
    .unwrap()
    .run()
    .await
    .unwrap();
}
