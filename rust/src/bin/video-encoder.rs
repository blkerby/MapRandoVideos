// use std::{collections::HashMap, fs, path::Path, sync::{Mutex, TryLockResult}};

// use actix_web::{self, guard::Not, middleware::Logger, post, web, App, HttpResponse, HttpServer, Responder};
// use anyhow::Result;
// use clap::Parser;
// use futures::pin_mut;
// use log::{info, error};
// use tokio_postgres::{
//     binary_copy::BinaryCopyInWriter,
//     types::{ToSql, Type},
// };
// use tokio::sync::Notify;

// #[derive(Parser)]
// struct Args {
//     #[arg(long, default_value_t = 8083)]
//     port: u16,
//     #[arg(long, env)]
//     postgres_host: String,
//     #[arg(long, env)]
//     postgres_db: String,
//     #[arg(long, env)]
//     postgres_user: String,
//     #[arg(long, env)]
//     postgres_password: String,
// }

// struct AppData {
//     db: deadpool_postgres::Pool,
//     notify: Notify,
// }

// async fn update_videos(app_data: &AppData) {

// }


// fn build_app_data() -> AppData {
//     let args = Args::parse();

//     let mut config = deadpool_postgres::Config::new();
//     config.host = Some(args.postgres_host.clone());
//     config.dbname = Some(args.postgres_db.clone());
//     config.user = Some(args.postgres_user.clone());
//     config.password = Some(args.postgres_password.clone());
//     let db_pool = config
//         .create_pool(
//             Some(deadpool_postgres::Runtime::Tokio1),
//             tokio_postgres::NoTls,
//         )
//         .unwrap();

//     let app_data = AppData {
//         db: db_pool,
//         notify: Notify::new(),
//     };

//     app_data
// }

// #[post("/update")]
// async fn update(app_data: web::Data<AppData>) -> impl Responder {
//      if let TryLockResult::Ok(_lock) = app_data.lock.try_lock() {

        
//      }
//      HttpResponse::Ok().body("")
// }

// async fn try_update(app_data: &AppData) -> Result<()> {
//     Ok(())
// }

// async fn update_loop(app_data: &AppData) -> ! {
//     loop {
//         match try_update(app_data).await {
//             Ok(()) => {},
//             Err(e) => {
//                 error!("Update failed: {}", e);
//             }
//         }

//         // Before attempting an update again, wait until we get a notification that new work is ready to be done.
//         app_data.notify.notified().await;
//     }
// }

// #[actix_web::main]
// async fn main() -> Result<()> {
//     env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
//         .format_timestamp_millis()
//         .init();

//     let args = Args::parse();

//     let app_data = actix_web::web::Data::new(build_app_data());

//     HttpServer::new(move || {
//         App::new()
//             .app_data(app_data.clone())
//             .wrap(Logger::default())
//             .service(update)
//     })
//     .workers(1)
//     .bind(("0.0.0.0", args.port))
//     .unwrap()
//     .run()
//     .await
//     .unwrap();

//     Ok(())
// }

fn main() {}
