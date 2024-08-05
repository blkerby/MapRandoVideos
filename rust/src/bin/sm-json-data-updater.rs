use std::{fs, path::Path, sync::Mutex};

use log::info;
use actix_web::{self, post, middleware::Logger, web, App, HttpResponse, HttpServer, Responder};
use clap::Parser;
use git2::Repository;
use object_store::{gcp::GoogleCloudStorageBuilder, local::LocalFileSystem, memory::InMemory, ObjectStore};
use serde::Serialize;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    git_repo_url: String,
    #[arg(long)]
    git_repo_branch: String,
    #[arg(long)]
    git_repo_local_path: String,
    #[arg(long)]
    object_store_url: String,
}

struct AppData {
    git_repository: Mutex<Repository>,
    git_branch: String,
    object_store: Box<dyn ObjectStore>,
}

fn update_repo(repo: &Repository, branch: &str) {
    let mut origin_remote = repo.find_remote("origin").unwrap();
    info!("Fetching updates on branch {}", branch);
    origin_remote.fetch(&[branch], None, None).unwrap();
    let oid = repo.refname_to_id(&format!("refs/remotes/origin/{}", branch)).unwrap();
    let object = repo.find_object(oid, None).unwrap();
    repo.reset(&object, git2::ResetType::Hard, None).unwrap();
}

pub fn create_repo(url: &str, branch: &str, path_str: &str) -> Repository {
    let path = Path::new(path_str);
    if !path.exists() {
        info!("Cloning repo {} into {}", url, path_str);
        Repository::clone(url, path).expect("Error cloning git repository")
    } else {
        info!("Opening existing repo at {}", path_str);
        let repo = Repository::open(path).expect("Error opening git repository");
        update_repo(&repo, branch);
        repo
    }
}

pub fn create_object_store(url: &str) -> Box<dyn ObjectStore> {
    let object_store: Box<dyn ObjectStore> = if url.starts_with("gs:") {
        Box::new(
            GoogleCloudStorageBuilder::from_env()
                .with_url(url)
                .build().unwrap(),
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

#[derive(Serialize)]
struct RoomData {
    node_listing: Vec<(i64, String)>,  // (node ID, node name)
    strat_listing: Vec<(i64, i64, String)>,  // (from node ID, to node ID, strat name)
}

async fn process_room(room_json: &serde_json::Value, object_store: &Box<dyn ObjectStore>) {
    let room_id = room_json["id"].as_i64().unwrap();

    let mut node_listing: Vec<(i64, String)> = vec![];
    for node_json in room_json["nodes"].as_array().unwrap() {
        let node_id = node_json["id"].as_i64().unwrap();
        let node_name = node_json["name"].as_str().unwrap().to_string();
        node_listing.push((node_id, node_name));
    }

    let mut strat_listing: Vec<(i64, i64, String)> = vec![];
    for strat_json in room_json["strats"].as_array().unwrap() {
        let link = strat_json["link"].as_array().unwrap();
        let from_node_id = link[0].as_i64().unwrap();
        let to_node_id = link[1].as_i64().unwrap();
        let strat_name = strat_json["name"].as_str().unwrap().to_string();
        strat_listing.push((from_node_id, to_node_id, strat_name));
    }

    let room_data = RoomData {
        node_listing,
        strat_listing
    };
    let room_data_str = serde_json::to_string(&room_data).unwrap();
    let object_path = object_store::path::Path::parse(format!("{}.json", room_id)).unwrap();
    object_store.put(&object_path, room_data_str.into()).await.unwrap();

}

async fn update_rooms(git_repo: &Repository, object_store: &Box<dyn ObjectStore>) {
    let region_pattern = git_repo.workdir().unwrap().to_str().unwrap().to_string() + "/region/**/*.json";
    info!("Processing rooms at {}, updating {}", region_pattern, object_store);
    let mut room_listing: Vec<(i64, String)> = vec![];
    for entry in glob::glob(&region_pattern).unwrap() {
        if let Ok(path) = entry {
            let path_str = path.to_str().unwrap();
            if path_str.contains("ceres") || path_str.contains("roomDiagrams") {
                continue;
            }

            let room_str = fs::read_to_string(path).unwrap();
            let room_json: serde_json::Value = serde_json::from_str(&room_str).unwrap();
            let room_id = room_json["id"].as_i64().unwrap();
            let room_name = room_json["name"].as_str().unwrap().to_string();
            room_listing.push((room_id, room_name));
            process_room(&room_json, object_store).await;
        }
    }
    let room_listing_str = serde_json::to_string(&room_listing).unwrap();
    let object_path = object_store::path::Path::parse("rooms.json").unwrap();
    object_store.put(&object_path, room_listing_str.into()).await.unwrap();
}

#[post("/update")]
async fn update_data(app_data: web::Data<AppData>) -> impl Responder {
    let git_repo = app_data.git_repository.lock().unwrap();
    update_repo(&git_repo, &app_data.git_branch);
    update_rooms(&git_repo, &app_data.object_store).await;
    HttpResponse::Ok().body("")
}

#[actix_web::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();

    let app_data = actix_web::web::Data::new(AppData {
        git_repository: Mutex::new(create_repo(&args.git_repo_url, &args.git_repo_branch, &args.git_repo_local_path)),
        git_branch: args.git_repo_branch,
        object_store: create_object_store(&args.object_store_url),
    });
    update_rooms(&app_data.git_repository.lock().unwrap(), &app_data.object_store).await;

    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .wrap(Logger::default())
            .service(update_data)
    })
    .bind("0.0.0.0:8082")
    .unwrap()
    .run()
    .await
    .unwrap();
}
