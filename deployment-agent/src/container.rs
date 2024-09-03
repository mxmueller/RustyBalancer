use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, ListContainersOptions, StartContainerOptions, StopContainerOptions, RemoveContainerOptions};
use bollard::models::{HostConfig, PortBinding, ContainerSummary as APIContainers};
use bollard::errors::Error;
use dotenv::dotenv;
use std::collections::HashMap;
use std::env;
use rand::Rng;
use bollard::image::CreateImageOptions;
use futures_util::stream::StreamExt;
use md5;
use redis::Commands;
use uuid::Uuid;
use crate::db;
use crate::queue::QueueItem;

pub fn generate_hash_based_key(app_identifier: &str, container_name: &str) -> String {
    let data = format!("{}:{}", app_identifier, container_name);
    let digest = md5::compute(data);
    format!("container:{:x}", digest)
}

fn store_container_info_init(
    conn: &mut redis::Connection,
    key: &str,
    port: u16,
    image: &str,
) -> redis::RedisResult<()> {
    let _: () = conn.hset(key, "category", "INIT")?; // LU, MU, HU, INIT, SUNDOWN
    let _: () = conn.hset(key, "port", port.to_string())?;
    let _: () = conn.hset(key, "image", image)?;

    Ok(())
}

pub fn update_container_category(
    conn: &mut redis::Connection,
    key: &str,
    category: &str,
) -> redis::RedisResult<()> {
    conn.hset(key, "category", category)
}

async fn pull_image(docker: &Docker, image_name: &str) -> Result<(), Error> {
    let create_image_options = CreateImageOptions {
        from_image: image_name,
        ..Default::default()
    };
    let mut stream = docker.create_image(Some(create_image_options), None, None);
    while let Some(pull_result) = stream.next().await {
        match pull_result {
            Ok(output) => println!("{:?}", output),
            Err(e) => eprintln!("Error while pulling image: {:?}", e),
        }
    }
    Ok(())
}

pub async fn create_container(
    container_name: &str,
    image_name: &str,
    target_port: u16,
    app_identifier: &str,
    conn: &mut redis::Connection,
) -> Result<String, Error> {
    let docker = Docker::connect_with_unix_defaults().expect("Failed to connect to Docker");

    pull_image(&docker, image_name).await?;

    let host_port: u16 = rand::thread_rng().gen_range(30000..40000);

    let port_bindings = {
        let mut map = HashMap::new();
        map.insert(
            format!("{}/tcp", target_port),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(host_port.to_string()),
            }]),
        );
        map
    };

    let host_config = HostConfig {
        port_bindings: Some(port_bindings),
        network_mode: Some("rust-network".to_string()),
        ..Default::default()
    };

    let mut labels = HashMap::new();
    labels.insert("application".to_string(), app_identifier.to_string());

    let config = Config {
        image: Some(image_name.to_string()),
        host_config: Some(host_config),
        labels: Some(labels),
        ..Default::default()
    };

    let create_options = CreateContainerOptions {
        name: container_name,
        ..Default::default()
    };

    let key = generate_hash_based_key(app_identifier, container_name);

    store_container_info_init(conn, &key, host_port, image_name)
        .map_err(|e| Error::from(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;

    let create_response = docker
        .create_container(Some(create_options), config)
        .await?;

    docker.start_container(&create_response.id, None::<StartContainerOptions<String>>).await?;

    Ok(container_name.to_string())
}

pub async fn create_single_container(
    image_name: &str,
    target_port: u16,
    app_identifier: &str,
    conn: &mut redis::Connection,
) -> Result<QueueItem, Error> {
    let uuid = Uuid::new_v4();
    let container_name = format!("worker-{}", &uuid.to_string()[..8]);

    match create_container(&container_name, image_name, target_port, app_identifier, conn).await {
        Ok(dns_name) => {
            let item = QueueItem {
                dns_name,
                score: 100.0,
                utilization_category: "LU".to_string(),
            };
            Ok(item)
        }
        Err(e) => Err(e)
    }
}

pub async fn list_running_containers(app_identifier: &str) -> Result<Vec<APIContainers>, Error> {
    let docker = Docker::connect_with_unix_defaults().expect("Failed to connect to Docker");

    let filters = {
        let mut map = HashMap::new();
        map.insert("label".to_string(), vec![format!("application={}", app_identifier)]);
        map
    };

    let options = ListContainersOptions {
        all: true,
        filters,
        ..Default::default()
    };

    let containers = docker.list_containers(Some(options)).await?;
    Ok(containers)
}

pub async fn check_and_stop_container_if_not_in_db(
    container: &APIContainers,
    conn: &mut redis::Connection,
    app_identifier: &str,
) -> Result<Option<QueueItem>, Error> {
    let docker = Docker::connect_with_unix_defaults().expect("Failed to connect to Docker");

    let container_name = container.names.as_ref()
        .and_then(|names| names.get(0).cloned())
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();

    let key = generate_hash_based_key(app_identifier, &container_name);

    if db::check_config_value_exists(conn, &key) {
        let category: String = conn.hget(&key, "category").unwrap_or_else(|_| "Unknown".to_string());
        Ok(Some(QueueItem {
            dns_name: container_name,
            score: 100.0,
            utilization_category: category,
        }))
    } else {
        docker.stop_container(container.id.as_deref().unwrap_or(""), None::<StopContainerOptions>).await?;
        docker.remove_container(container.id.as_deref().unwrap_or(""), None::<RemoveContainerOptions>).await?;
        Ok(None)
    }
}

pub fn cleanup_orphaned_db_entries(
    conn: &mut redis::Connection,
    running_container_keys: &HashMap<String, String>,
) -> Result<(), Error> {
    let keys: Vec<String> = conn.keys("container:*")
        .map_err(|e| Error::from(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;

    for key in keys {
        if !running_container_keys.contains_key(&key) {
            println!("Deleting orphaned database entry '{}'", key);
            let _: () = conn.del(&key)
                .map_err(|e| Error::from(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;
        }
    }

    Ok(())
}

pub async fn manage_containers(app_identifier: &str, default_container: i16) -> Result<Vec<QueueItem>, Error> {
    let mut conn = db::get_redis_connection();
    let image_name = env::var("DOCKER_IMAGE").expect("DOCKER_IMAGE must be set");
    let target_port: u16 = env::var("TARGET_PORT")
        .expect("TARGET_PORT must be set")
        .parse()
        .expect("TARGET_PORT must be a valid number");

    let containers = list_running_containers(app_identifier).await?;

    let mut running_containers = HashMap::new();
    let mut queue_items = Vec::new();

    for container in containers {
        if let Some(item) = check_and_stop_container_if_not_in_db(&container, &mut conn, app_identifier).await? {
            if let Some(id) = &container.id {
                running_containers.insert(generate_hash_based_key(app_identifier, &item.dns_name), id.clone());
            }
            queue_items.push(item);
        }
    }

    cleanup_orphaned_db_entries(&mut conn, &running_containers)?;

    let running_containers_count = running_containers.len() as i16;
    if running_containers_count < default_container {
        for _ in running_containers_count + 1..=default_container {
            match create_single_container(&image_name, target_port, app_identifier, &mut conn).await {
                Ok(item) => {
                    queue_items.push(item);
                }
                Err(e) => eprintln!("Failed to create container: {:?}", e),
            }
        }
    }

    Ok(queue_items)
}

pub async fn remove_container(app_identifier: &str, container_name: &str) -> Result<(), Error> {
    let docker = Docker::connect_with_unix_defaults().expect("Failed to connect to Docker");

    // First, stop the container
    docker.stop_container(container_name, None::<StopContainerOptions>).await?;

    // Then, remove the container
    docker.remove_container(
        container_name,
        Some(RemoveContainerOptions {
            force: true,
            ..Default::default()
        }),
    ).await?;

    // Remove container info from Redis
    let mut conn = db::get_redis_connection();
    let key = generate_hash_based_key(app_identifier, container_name);
    let _: () = conn.del(&key).map_err(|e| Error::from(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;

    println!("Container {} has been removed", container_name);

    Ok(())
}