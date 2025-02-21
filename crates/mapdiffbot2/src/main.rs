mod gc_job;
mod git_operations;
mod github_processor;
mod job_processor;
mod rendering;
mod runner;

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use diffbot_lib::async_mutex::Mutex;
use once_cell::sync::OnceCell;
use serde::Deserialize;
use std::sync::Arc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub type DataJobSender = actix_web::web::Data<Arc<Mutex<diffbot_lib::job::types::JobSender>>>;

#[actix_web::get("/")]
async fn index() -> &'static str {
    "MDB says hello!"
}

#[derive(Debug, Deserialize)]
pub struct GithubConfig {
    pub app_id: u64,
    pub private_key_path: String,
}

#[derive(Debug, Deserialize)]
pub struct WebLimitsConfig {
    pub forms: usize,
    pub string: usize,
}

#[derive(Debug, Deserialize)]
pub struct WebConfig {
    pub address: String,
    pub port: u16,
    pub file_hosting_url: String,
    pub limits: Option<WebLimitsConfig>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub github: GithubConfig,
    pub web: WebConfig,
    #[serde(default = "std::collections::HashSet::new")]
    pub blacklist: std::collections::HashSet<u64>,
    #[serde(default = "String::new")]
    pub blacklist_contact: String,
    #[serde(default = "default_schedule")]
    pub gc_schedule: String,
    #[serde(default = "default_log_level")]
    pub logging: String,
}

fn default_schedule() -> String {
    "0 0 4 * * *".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

static CONFIG: OnceCell<Config> = OnceCell::new();

fn read_key(path: PathBuf) -> Vec<u8> {
    let mut key_file =
        File::open(&path).unwrap_or_else(|_| panic!("Unable to find file {}", path.display()));

    let mut key = Vec::new();
    let _ = key_file
        .read_to_end(&mut key)
        .unwrap_or_else(|_| panic!("Failed to read key {}", path.display()));

    key
}

fn init_config(path: &std::path::Path) -> eyre::Result<&'static Config> {
    let mut config_str = String::new();
    File::open(path)?.read_to_string(&mut config_str)?;

    let config = toml::from_str(&config_str)?;

    CONFIG.set(config).expect("Failed to set config");
    Ok(CONFIG.get().unwrap())
}

const JOB_JOURNAL_LOCATION: &str = "jobs";

#[actix_web::main]
async fn main() -> eyre::Result<()> {
    stable_eyre::install().expect("Eyre handler installation failed!");

    let config_path = std::path::Path::new(".").join("config.toml");
    let config =
        init_config(&config_path).unwrap_or_else(|_| panic!("Failed to read {:?}", config_path));

    diffbot_lib::logger::init_logger(&config.logging).expect("Log init failed!");

    let key = read_key(PathBuf::from(&config.github.private_key_path));

    octocrab::initialise(octocrab::OctocrabBuilder::new().app(
        config.github.app_id.into(),
        jsonwebtoken::EncodingKey::from_rsa_pem(&key).unwrap(),
    ))
    .expect("fucked up octocrab");

    let (job_sender, job_receiver) = yaque::channel(JOB_JOURNAL_LOCATION)
        .expect("Couldn't open an on-disk queue, check permissions or drive space?");

    actix_web::rt::spawn(runner::handle_jobs("MapDiffBot2", job_receiver));

    let job_sender = Arc::new(Mutex::new(job_sender));

    let job_clone = job_sender.clone();

    let cron_str = config.gc_schedule.to_owned();

    actix_web::rt::spawn(async move { gc_job::gc_scheduler(cron_str, job_clone).await });

    actix_web::HttpServer::new(move || {
        use actix_web::web::{FormConfig, PayloadConfig};
        //absolutely rancid
        let (form_config, string_config) = config.web.limits.as_ref().map_or(
            (FormConfig::default(), PayloadConfig::default()),
            |limits| {
                (
                    FormConfig::default().limit(limits.forms),
                    PayloadConfig::default().limit(limits.string),
                )
            },
        );

        actix_web::App::new()
            .app_data(form_config)
            .app_data(string_config)
            .app_data(actix_web::web::Data::new(job_sender.clone()))
            .service(index)
            .service(github_processor::process_github_payload)
            .service(actix_files::Files::new("/images", "./images"))
    })
    .bind((config.web.address.as_ref(), config.web.port))?
    .run()
    .await?;
    Ok(())
}
