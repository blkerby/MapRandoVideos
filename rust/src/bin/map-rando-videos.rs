use actix_web::{self, get, middleware::{Compress, Logger}, web, App, HttpResponse, HttpServer, Responder};
use askama::Template;

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {}

struct AppData {

}

fn build_app_data() -> AppData {
    AppData {}
}

#[get("/")]
async fn home(app_data: web::Data<AppData>) -> impl Responder {
    let home_template = HomeTemplate {};
    HttpResponse::Ok().body(home_template.render().unwrap())
}

#[actix_web::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let app_data = actix_web::web::Data::new(build_app_data());

    HttpServer::new(move || {
        App::new()
            .wrap(Compress::default())
            .app_data(app_data.clone())
            .wrap(Logger::default())
            .service(home)
            .service(actix_files::Files::new("/js", "../js"))
    })
    .bind("0.0.0.0:8081")
    .unwrap()
    .run()
    .await
    .unwrap();
}
