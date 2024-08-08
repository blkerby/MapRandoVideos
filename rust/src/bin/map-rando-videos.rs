use actix_web::{
    self, get,
    middleware::{Compress, Logger},
    post, web, App, HttpResponse, HttpServer, Responder,
};
use actix_web_httpauth::extractors::basic::BasicAuth;
use anyhow::{bail, Context, Result};
use askama::Template;
use clap::Parser;
use log::{error, info};
use serde::Deserialize;
use std::str::FromStr;

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
    video_storage_url: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    sm_json_data_summary_url: String,
}

struct AppData {
    args: Args,
    db: deadpool_postgres::Pool,
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
    AppData { args, db: pool }
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
            info!("token: {}, permission: {}", token, permission_str);
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
            error!("Failed sign-in: {}", e);
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
            .service(submit_video)
            .service(actix_files::Files::new("/js", "../js"))
    })
    .bind("0.0.0.0:8081")
    .unwrap()
    .run()
    .await
    .unwrap();
}
