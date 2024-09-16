use std::{collections::HashMap, fs, path::Path, sync::Mutex};

use actix_web::{self, middleware::Logger, post, web, App, HttpResponse, HttpServer, Responder};
use anyhow::{Context, Result};
use clap::Parser;
use futures::pin_mut;
use git2::Repository;
use log::info;
use tokio_postgres::{
    binary_copy::BinaryCopyInWriter,
    types::{ToSql, Type},
};

#[derive(Parser)]
struct Args {
    #[arg(long, env)]
    git_repo_url: String,
    #[arg(long, env)]
    git_repo_branch: String,
    #[arg(long, env)]
    git_repo_local_path: String,
    #[arg(long, env)]
    postgres_host: String,
    #[arg(long, env)]
    postgres_db: String,
    #[arg(long, env)]
    postgres_user: String,
    #[arg(long, env)]
    postgres_password: String,
}

struct AppData {
    git_repository: Mutex<Repository>,
    git_branch: String,
    db: deadpool_postgres::Pool,
}

struct SMJsonDataSummary {
    areas: Vec<AreaData>,
    rooms: Vec<RoomData>,
    nodes: Vec<NodeData>,
    strats: Vec<StratData>,
    techs: Vec<TechData>,
    notables: Vec<NotableData>,
    notable_strats: Vec<NotableStratData>,
}

struct AreaData {
    area_id: i32,
    name: String,
}

struct RoomData {
    room_id: i32,
    area_id: i32,
    name: String,
}

struct NodeData {
    room_id: i32,
    node_id: i32,
    name: String,
}

struct StratData {
    room_id: i32,
    strat_id: i32,
    from_node_id: i32,
    to_node_id: i32,
    name: String,
}

struct TechData {
    tech_id: i32,
    name: String,
}

struct NotableData {
    room_id: i32,
    notable_id: i32,
    name: String,
}

struct NotableStratData {
    room_id: i32,
    notable_id: i32,
    strat_id: i32,
}

fn update_repo(repo: &Repository, branch: &str) {
    let mut origin_remote = repo.find_remote("origin").unwrap();
    info!("Fetching updates on branch {}", branch);
    origin_remote.fetch(&[branch], None, None).unwrap();
    let oid = repo
        .refname_to_id(&format!("refs/remotes/origin/{}", branch))
        .unwrap();
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

fn get_area_order() -> Vec<String> {
    vec![
        "Central Crateria",
        "West Crateria",
        "East Crateria",
        "Blue Brinstar",
        "Green Brinstar",
        "Pink Brinstar",
        "Red Brinstar",
        "Kraid Brinstar",
        "East Upper Norfair",
        "West Upper Norfair",
        "Crocomire Upper Norfair",
        "West Lower Norfair",
        "East Lower Norfair",
        "Wrecked Ship",
        "Outer Maridia",
        "Pink Inner Maridia",
        "Yellow Inner Maridia",
        "Green Inner Maridia",
        "Tourian",
    ]
    .into_iter()
    .map(|x| x.to_string())
    .collect()
}

fn get_area(room_json: &serde_json::Value) -> String {
    let area = room_json["area"].as_str().unwrap().to_string();
    let sub_area = room_json["subarea"].as_str().unwrap_or("").to_string();
    let sub_sub_area = room_json["subsubarea"].as_str().unwrap_or("").to_string();
    let full_area = if sub_sub_area != "" {
        format!("{} {} {}", sub_sub_area, sub_area, area)
    } else if sub_area != "" && sub_area != "Main" {
        format!("{} {}", sub_area, area)
    } else {
        area
    };
    full_area
}

fn process_tech_rec(tech_json: &serde_json::Value, out: &mut Vec<TechData>) -> Result<()> {
    if tech_json.get("id").is_none() {
        info!(
            "Skipping tech without 'id': {}",
            tech_json["name"].as_str().unwrap()
        );
        return Ok(());
    }
    out.push(TechData {
        tech_id: tech_json["id"].as_i64().context("Missing 'id'")? as i32,
        name: tech_json["name"]
            .as_str()
            .context("Missing 'name'")?
            .to_owned(),
    });
    for t in tech_json["extensionTechs"].as_array().unwrap_or(&vec![]) {
        process_tech_rec(t, out)?;
    }
    Ok(())
}

fn process_requirement(
    req: &serde_json::Value,
    room_id: i32,
    strat_id: i32,
    notable_map: &HashMap<String, i32>,
    notable_strats: &mut Vec<NotableStratData>,
) {
    if req.is_object() {
        let obj = req.as_object().unwrap();
        if let Some(val) = obj.get("notable") {
            let notable_name = val.as_str().unwrap().to_string();
            let notable_id = notable_map[&notable_name];
            notable_strats.push(NotableStratData {
                room_id,
                notable_id,
                strat_id,
            });
        } else if let Some(val) = obj.get("or") {
            for r in val.as_array().unwrap() {
                process_requirement(r, room_id, strat_id, notable_map, notable_strats);
            }
        } else if let Some(val) = obj.get("and") {
            for r in val.as_array().unwrap() {
                process_requirement(r, room_id, strat_id, notable_map, notable_strats);
            }
        }
    }
}

fn load_sm_data_summary(git_repo: &Repository) -> Result<SMJsonDataSummary> {
    let area_names = get_area_order();
    let mut area_map: HashMap<String, i32> = HashMap::new();
    let mut areas: Vec<AreaData> = vec![];
    let mut rooms: Vec<RoomData> = vec![];
    let mut nodes: Vec<NodeData> = vec![];
    let mut strats: Vec<StratData> = vec![];
    let mut notables: Vec<NotableData> = vec![];
    let mut notable_strats: Vec<NotableStratData> = vec![];

    for (i, name) in area_names.iter().enumerate() {
        areas.push(AreaData {
            area_id: i as i32,
            name: name.to_string(),
        });
        area_map.insert(name.clone(), i as i32);
    }

    let region_pattern =
        git_repo.workdir().unwrap().to_str().unwrap().to_string() + "/region/**/*.json";
    for entry in glob::glob(&region_pattern).unwrap() {
        if let Ok(path) = entry {
            let path_str = path.to_str().unwrap();
            if path_str.contains("ceres") || path_str.contains("roomDiagrams") {
                continue;
            }

            let room_str = fs::read_to_string(path).unwrap();
            let room_json: serde_json::Value = serde_json::from_str(&room_str).unwrap();
            let room_id = room_json["id"].as_i64().unwrap() as i32;
            let area_name = get_area(&room_json);
            let area_id = area_map[&area_name];
            rooms.push(RoomData {
                room_id,
                area_id,
                name: room_json["name"].as_str().unwrap().to_string(),
            });

            for node_json in room_json["nodes"].as_array().unwrap() {
                let node_id = node_json["id"].as_i64().unwrap() as i32;
                let node_name = node_json["name"].as_str().unwrap().to_string();
                nodes.push(NodeData {
                    room_id,
                    node_id,
                    name: node_name,
                });
            }

            let mut notable_map: HashMap<String, i32> = HashMap::new();

            for notable_json in room_json["notables"].as_array().unwrap() {
                let notable_id = notable_json["id"].as_i64().unwrap() as i32;
                let name = notable_json["name"].as_str().unwrap().to_string();
                notable_map.insert(name.clone(), notable_id);
                notables.push(NotableData {
                    room_id,
                    notable_id,
                    name,
                });
            }

            for strat_json in room_json["strats"].as_array().unwrap() {
                let strat_id = strat_json["id"].as_i64().unwrap_or(0) as i32;
                if strat_id == 0 {
                    // Skip strats that don't yet have an ID assigned.
                    continue;
                }
                let link = strat_json["link"].as_array().unwrap();
                let from_node_id = link[0].as_i64().unwrap() as i32;
                let to_node_id = link[1].as_i64().unwrap() as i32;
                let strat_name = strat_json["name"].as_str().unwrap().to_string();
                strats.push(StratData {
                    room_id,
                    strat_id,
                    from_node_id,
                    to_node_id,
                    name: strat_name,
                });

                for req in strat_json["requires"].as_array().unwrap() {
                    process_requirement(req, room_id, strat_id, &notable_map, &mut notable_strats);
                }
            }
        }
    }

    let tech_path = git_repo.workdir().unwrap().to_str().unwrap().to_string() + "tech.json";
    let tech_str = fs::read_to_string(tech_path).unwrap();
    let tech_json: serde_json::Value = serde_json::from_str(&tech_str).unwrap();
    let mut techs: Vec<TechData> = vec![];

    for tech_category in tech_json["techCategories"].as_array().unwrap() {
        for tech_json in tech_category["techs"].as_array().unwrap() {
            process_tech_rec(&tech_json, &mut techs)
                .with_context(|| format!("Processing tech {:?}", tech_json["name"].as_str()))?;
        }
    }

    Ok(SMJsonDataSummary {
        areas,
        rooms,
        nodes,
        strats,
        techs,
        notables,
        notable_strats,
    })
}

async fn write_area_table(app_data: &AppData, areas: &[AreaData]) -> Result<()> {
    let mut db = app_data.db.get().await?;
    let tran = db.transaction().await?;
    let stmt = tran.prepare_cached("TRUNCATE TABLE area").await?;
    tran.execute(&stmt, &[]).await?;
    let sink = tran
        .copy_in("COPY area (area_id, name) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::VARCHAR]);
    pin_mut!(writer);
    let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
    for area in areas {
        row.clear();
        row.push(&area.area_id);
        row.push(&area.name);
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;
    tran.commit().await?;
    Ok(())
}

async fn write_room_table(app_data: &AppData, rooms: &[RoomData]) -> Result<()> {
    let mut db = app_data.db.get().await?;
    let tran = db.transaction().await?;
    let stmt = tran.prepare_cached("TRUNCATE TABLE room").await?;
    tran.execute(&stmt, &[]).await?;
    let sink = tran
        .copy_in("COPY room (room_id, area_id, name) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::INT4, Type::VARCHAR]);
    pin_mut!(writer);
    let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
    for room in rooms {
        row.clear();
        row.push(&room.room_id);
        row.push(&room.area_id);
        row.push(&room.name);
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;
    tran.commit().await?;
    Ok(())
}

async fn write_node_table(app_data: &AppData, nodes: &[NodeData]) -> Result<()> {
    let mut db = app_data.db.get().await?;
    let tran = db.transaction().await?;
    let stmt = tran.prepare_cached("TRUNCATE TABLE node").await?;
    tran.execute(&stmt, &[]).await?;
    let sink = tran
        .copy_in("COPY node (room_id, node_id, name) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::INT4, Type::VARCHAR]);
    pin_mut!(writer);
    let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
    for node in nodes {
        row.clear();
        row.push(&node.room_id);
        row.push(&node.node_id);
        row.push(&node.name);
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;
    tran.commit().await?;
    Ok(())
}

async fn write_strat_table(app_data: &AppData, nodes: &[StratData]) -> Result<()> {
    let mut db = app_data.db.get().await?;
    let tran = db.transaction().await?;
    let stmt = tran.prepare_cached("TRUNCATE TABLE strat").await?;
    tran.execute(&stmt, &[]).await?;
    let sink = tran
        .copy_in("COPY strat (room_id, strat_id, from_node_id, to_node_id, name) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(
        sink,
        &[
            Type::INT4,
            Type::INT4,
            Type::INT4,
            Type::INT4,
            Type::VARCHAR,
        ],
    );
    pin_mut!(writer);
    let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
    for node in nodes {
        row.clear();
        row.push(&node.room_id);
        row.push(&node.strat_id);
        row.push(&node.from_node_id);
        row.push(&node.to_node_id);
        row.push(&node.name);
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;
    tran.commit().await?;
    Ok(())
}

async fn write_tech_table(app_data: &AppData, techs: &[TechData]) -> Result<()> {
    let mut db = app_data.db.get().await?;
    let tran = db.transaction().await?;
    let stmt = tran.prepare_cached("TRUNCATE TABLE tech").await?;
    tran.execute(&stmt, &[]).await?;
    let sink = tran
        .copy_in("COPY tech (tech_id, name) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::VARCHAR]);
    pin_mut!(writer);
    let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
    for tech in techs {
        row.clear();
        row.push(&tech.tech_id);
        row.push(&tech.name);
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;
    tran.commit().await?;
    Ok(())
}

async fn write_notable_table(app_data: &AppData, notables: &[NotableData]) -> Result<()> {
    let mut db = app_data.db.get().await?;
    let tran = db.transaction().await?;
    let stmt = tran.prepare_cached("TRUNCATE TABLE notable").await?;
    tran.execute(&stmt, &[]).await?;
    let sink = tran
        .copy_in("COPY notable (room_id, notable_id, name) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::INT4, Type::VARCHAR]);
    pin_mut!(writer);
    let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
    for notable in notables {
        row.clear();
        row.push(&notable.room_id);
        row.push(&notable.notable_id);
        row.push(&notable.name);
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;
    tran.commit().await?;
    Ok(())
}

async fn write_notable_strat_table(app_data: &AppData, notable_strats: &[NotableStratData]) -> Result<()> {
    let mut db = app_data.db.get().await?;
    let tran = db.transaction().await?;
    let stmt = tran.prepare_cached("TRUNCATE TABLE notable_strat").await?;
    tran.execute(&stmt, &[]).await?;
    let sink = tran
        .copy_in("COPY notable_strat (room_id, notable_id, strat_id) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::INT4, Type::INT4]);
    pin_mut!(writer);
    let mut row: Vec<&'_ (dyn ToSql + Sync)> = Vec::new();
    for notable_strat in notable_strats {
        row.clear();
        row.push(&notable_strat.room_id);
        row.push(&notable_strat.notable_id);
        row.push(&notable_strat.strat_id);
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;
    tran.commit().await?;
    Ok(())
}

async fn update_incomplete_videos(app_data: &AppData) -> Result<()> {
    // Change videos with invalid or inconsistent IDs to "Incomplete" status.
    // Except videos that are already "Disabled" are left alone.
    let db = app_data.db.get().await?;
    let sql = r#"
        WITH invalid_ids AS (
            SELECT v.id
            FROM video v
            LEFT JOIN room r ON v.room_id = r.room_id 
            LEFT JOIN node f ON v.room_id = f.room_id AND v.from_node_id = f.node_id
            LEFT JOIN node t ON v.room_id = t.room_id AND v.to_node_id = t.node_id
            LEFT JOIN strat s
              ON v.room_id = s.room_id 
              AND v.strat_id = s.strat_id
              AND v.from_node_id = s.from_node_id 
              AND v.to_node_id = s.to_node_id
            WHERE r.room_id IS NULL OR f.node_id IS NULL OR t.node_id IS NULL OR s.strat_id IS NULL
        )
        UPDATE video SET status = 'Incomplete'
        WHERE id IN (SELECT id FROM invalid_ids) AND status != 'Disabled'
    "#;
    let stmt = db.prepare_cached(&sql).await?;
    let cnt = db.execute(&stmt, &[]).await?;
    info!("{} video(s) updated to 'Incomplete'", cnt);
    Ok(())
}

async fn update_tables(git_repo: &Repository, app_data: &AppData) -> Result<()> {
    info!("Loading sm-json-data summary");
    let summary = load_sm_data_summary(git_repo)?;
    info!("Rewriting database tables");
    write_area_table(app_data, &summary.areas).await?;
    write_room_table(app_data, &summary.rooms).await?;
    write_node_table(app_data, &summary.nodes).await?;
    write_strat_table(app_data, &summary.strats).await?;
    write_tech_table(app_data, &summary.techs).await?;
    write_notable_table(app_data, &summary.notables).await?;
    write_notable_strat_table(app_data, &summary.notable_strats).await?;
    update_incomplete_videos(app_data).await?;
    info!("Successfully rewrote tables");
    Ok(())
}

#[post("/update")]
async fn update_data(app_data: web::Data<AppData>) -> impl Responder {
    let git_repo = app_data.git_repository.lock().unwrap();
    update_repo(&git_repo, &app_data.git_branch);
    update_tables(&git_repo, &app_data).await.unwrap();
    HttpResponse::Ok().body("")
}

fn build_app_data() -> AppData {
    let args = Args::parse();

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

    let app_data = AppData {
        git_repository: Mutex::new(create_repo(
            &args.git_repo_url,
            &args.git_repo_branch,
            &args.git_repo_local_path,
        )),
        git_branch: args.git_repo_branch,
        db: db_pool,
    };

    app_data
}

#[actix_web::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let app_data = actix_web::web::Data::new(build_app_data());
    update_tables(&app_data.git_repository.lock().unwrap(), &app_data).await?;

    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .wrap(Logger::default())
            .service(update_data)
    })
    .workers(1)
    .bind("0.0.0.0:8082")
    .unwrap()
    .run()
    .await
    .unwrap();

    Ok(())
}
