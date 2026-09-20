use super::*;
use std::sync::{Arc, atomic::{AtomicU8, Ordering}};
use axum::{routing::{get, post}, Router};
use base64::Engine;

async fn listener(app: Router) -> Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    Ok((url, handle))
}

/// Real PostgreSQL and HTTP observer/health requests, but no cloud VM creation.
/// Published candidates simulate the durable result left by a prior worker.
#[tokio::test]
#[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
async fn regional_rollout_postgres_restart_drain_and_rollback() -> Result<()> {
    let mode = Arc::new(AtomicU8::new(0)); // 1: US observer stale; 2: EU requests remain
    let probe_failure = Arc::new(AtomicU8::new(0));
    let mut servers = Vec::new();
    let mut endpoints = Vec::new();
    for _ in 0..6 {
        let failure = probe_failure.clone();
        let (url, task) = listener(Router::new().route("/health", get(move || {
            let failure = failure.clone();
            async move { if failure.load(Ordering::SeqCst) == 0 { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE } }
        }))).await?;
        endpoints.push(url);
        servers.push(task);
    }
    let service = format!("regional-test-{}", uuid::Uuid::new_v4().simple());
    let observer_service = service.clone();
    let observer_mode = mode.clone();
    let token = uuid::Uuid::new_v4().to_string();
    let secret = base64::engine::general_purpose::STANDARD.encode(&token);
    let (base_url, task) = listener(Router::new()
        .route("/v1/secrets/read", post(move || {
            let secret = secret.clone();
            async move { Json(json!({"path":"test/observer","version":1,"status":"active",
                "valueBase64":secret,"createdAt":Utc::now(),"metadata":{}})) }
        }))
        .route("/deployments/{region}/discovery-status", get(move |Path(region): Path<String>| {
            let service = observer_service.clone();
            let mode = observer_mode.clone();
            async move {
                let s = snapshot(&service).await.unwrap();
                let mut upstreams = Vec::new();
                for e in &s.endpoints {
                    let url = reqwest::Url::parse(&e.url).unwrap();
                    let peer = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
                    if !e.draining {
                        upstreams.push(json!({"peer":peer,"draining":false,"inFlight":0}));
                    } else if mode.load(Ordering::SeqCst) == 2 && region == "eu" {
                        upstreams.push(json!({"peer":peer,"draining":true,"inFlight":1}));
                    }
                }
                let version = if mode.load(Ordering::SeqCst) == 1 && region == "us" { 0 } else { s.version };
                Json(json!({"serviceId":service,"version":version,"upstreams":upstreams}))
            }
        }))).await?;
    servers.push(task);
    let config: crate::config::Config = serde_json::from_value(json!({
        "server_port":0,"database_url":std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?,
        "agent_provider":"test","agent_model":"test","agent_api_key":"",
        "agent_timeout_seconds":1,"agent_max_iterations":1,"jwt_secret":token,
        "cloud_internal_url":base_url,"internal_api_key":token,"heyosecret_url":base_url,
        "discovery_routed_services":service,
        "discovery_observers":[
            {"service_id":service,"region":"eu","deployment_id":"eu","base_url":base_url,"token_secret_path":"test/observer"},
            {"service_id":service,"region":"us","deployment_id":"us","base_url":base_url,"token_secret_path":"test/observer"}
        ]
    }))?;
    db::init_database(&config).await?;
    let db = db::get_db()?;
    // Assert the new migration succeeds, including repeat application. Do not
    // rely on the existing startup migration runner's warning-only handling.
    db.execute_unprepared(include_str!("../../migrations/035_add_regional_service_rollouts.sql")).await?;
    let state = AppState { config: Arc::new(config), http_client: reqwest::Client::new(),
        worker_id: Arc::new("test".into()), ci_workspace_cache: Default::default() };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse()?);
    let req = RegionalRolloutRequest { operation_id: service.clone(),
        deployment: serde_json::from_value(json!({"serviceId":service,"userId":"test",
            "archiveId":"immutable-test","desiredReplicas":2,"replicaRegions":["eu","us"],
            "route":{"host":"test.example","pathPrefix":"/"}}))?,
        minimum_serving_replicas:1,bake_seconds:10,drain_timeout_seconds:60,runtime_by_region:HashMap::new() };
    let slots: Vec<_> = ["eu","us"].iter().enumerate().map(|(index, region)| Slot {
        index,region:region.to_string(),candidate_id:candidate_id(&req.operation_id,index),
        runtime: None,
    }).collect();
    let baseline = service_deploy::ServiceDeploymentState {
        service_id: service.clone(), active_deployment_id: Some("old-us".into()),
        desired_replicas: 2, replica_regions: vec!["eu".into(),"us".into()],
        ..Default::default()
    };
    let plan = Plan::compile(&["eu".into(),"us".into()], &slots.iter()
        .map(|s| (s.region.clone(), s.candidate_id.clone())).collect::<Vec<_>>());
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,
         observer_topology,regions,slots,baseline_state,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
         VALUES($1,$1,$2,$3,'new',$4,$5,$6,$7,$8,1,10,60,'running','preflight')",
        vec![service.clone().into(),payload_hash(&req)?.into(),serde_json::to_value(&req.deployment)?.into(),
            super::super::regional_observers::topology(&state,&service)?.into(),json!(["eu","us"]).into(),serde_json::to_value(&slots)?.into(),serde_json::to_value(&baseline)?.into(),serde_json::to_value(&plan)?.into()])).await?;
    let admitted = load(Some(db), &service).await?.unwrap();
    assert_eq!(admitted.items.as_array().unwrap().len(), plan.steps.len() + plan.rollback_steps.len());
    assert_eq!(admitted.events.as_array().unwrap().len(), 1);
    assert_eq!(admitted.items.as_array().unwrap().iter().filter(|i| i["status"] == "running").count(), 1);
    // Neither plan edits nor policy edits are allowed under an existing ID.
    for assignment in ["plan=jsonb_set(plan,'{version}','2')", "bake_seconds=11"] {
        assert!(db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            format!("UPDATE regional_service_rollouts SET {assignment} WHERE operation_id=$1"),
            [service.clone().into()])).await.is_err());
    }
    assert!(db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET phase='bake',slot_index=1 WHERE operation_id=$1",
        [service.clone().into()])).await.is_err());
    let unchanged = load(Some(db), &service).await?.unwrap();
    assert_eq!(unchanged.phase,"preflight");
    assert_eq!(unchanged.items,admitted.items);
    assert_eq!(unchanged.events,admitted.events,"rejected transition must not leave partial progress/history");
    let (status, Json(body)) = super::get(headers.clone(),State(state.clone()),Path(service.clone())).await;
    assert_eq!(status,StatusCode::OK);
    assert_eq!(body["plan"],serde_json::to_value(&plan)?);
    assert_eq!(body["items"],admitted.items);
    for (index, region) in ["eu","us"].iter().enumerate() {
        service_discovery::publish_healthy_endpoint(&service, &format!("old-{region}"), None,
            Some(region), Some("old"), &endpoints[index], false).await?;
    }
    assert_eq!(create(headers.clone(), State(state.clone()), Json(req.clone())).await.0, StatusCode::OK);
    let mut different = req.clone(); different.bake_seconds += 1;
    assert_eq!(create(headers.clone(), State(state.clone()), Json(different)).await.0, StatusCode::CONFLICT);
    assert!(ensure_no_regional_rollout(db,&service).await.is_err());
    let lock = service_deploy::try_service_lifecycle_lock(db,&service).await?.unwrap();
    tick(&state,&service).await?;
    assert_eq!(load(Some(db),&service).await?.unwrap().phase,"preflight");
    lock.rollback().await?;
    tick(&state,&service).await?; // preflight
    tick(&state,&service).await?; // exclude EU
    let excluded = snapshot(&service).await?;
    assert!(excluded.endpoints.iter().find(|e|e.deployment_id=="old-eu").unwrap().draining);
    assert!(!excluded.endpoints.iter().find(|e|e.deployment_id=="old-us").unwrap().draining);
    let stored = service_discovery::read_stored_snapshot(&service).await?.unwrap();
    assert!(!stored.endpoints.iter().find(|e|e.deployment_id=="old-eu").unwrap().draining);
    service_discovery::restore_snapshot(&service,Some(&stored)).await?;
    mode.store(1,Ordering::SeqCst);
    tick(&state,&service).await?;
    assert_eq!(load(Some(db),&service).await?.unwrap().phase,"wait_drained");
    mode.store(2,Ordering::SeqCst);
    tick(&state,&service).await?;
    assert_eq!(load(Some(db),&service).await?.unwrap().phase,"wait_drained");
    mode.store(0,Ordering::SeqCst);
    tick(&state,&service).await?;
    let r = load(Some(db),&service).await?.unwrap();
    assert_eq!(r.phase,"create_slot");
    // Crash after journaling intent but before confirming creation. A new
    // worker must block, not call Cloud again or advance the regional cursor.
    update(db,&r,"creating",0,None,None,None).await?;
    tick(&state,&service).await?;
    let blocked = load(Some(db),&service).await?.unwrap();
    assert_eq!(blocked.status,"blocked");
    assert_eq!(blocked.phase,"creating");
    assert_eq!(blocked.region_index,0);
    let item = blocked.items.as_array().unwrap().iter().find(|i| i["stepId"] == "0:creating:0").unwrap();
    assert_eq!(item["status"], "blocked");
    assert!(item["error"].as_str().unwrap().contains("uncertain candidate"));
    // A published success left by the prior worker is safe to adopt.
    service_discovery::publish_healthy_endpoint(&service,&slots[0].candidate_id,None,
        Some("eu"),Some("new"),&endpoints[2],false).await?;
    assert!(snapshot(&service).await?.endpoints.iter().find(|e|e.deployment_id==slots[0].candidate_id).unwrap().draining);
    assert_eq!(resume(headers.clone(),State(state.clone()),Path(service.clone())).await.0,StatusCode::ACCEPTED);
    for _ in 0..6 { tick(&state,&service).await?; }
    assert_eq!(load(Some(db),&service).await?.unwrap().phase,"bake");
    // Downtime is not credited as successful observation.
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET deadline_at=NOW()-INTERVAL '1 second',last_observed_at=NOW()-INTERVAL '1 hour' WHERE operation_id=$1",
        [service.clone().into()])).await?;
    tick(&state,&service).await?;
    assert_eq!(load(Some(db),&service).await?.unwrap().phase,"bake");
    probe_failure.store(1,Ordering::SeqCst);
    tick(&state,&service).await?;
    assert_eq!(load(Some(db),&service).await?.unwrap().status,"blocked");
    assert_eq!(load(Some(db),&service).await?.unwrap().region_index,0);
    probe_failure.store(0,Ordering::SeqCst);
    // A prior ordinary deployment may have left a retirement intent for a
    // replica other than the scalar active_deployment_id. Rollback must cancel
    // that intent without cancelling unrelated cleanup from the same run.
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO service_deployment_runs(deployment_id,service_id,status,phase) VALUES($1,$1,'passed','complete')",
        [service.clone().into()])).await?;
    for previous in ["old-eu", "unrelated-old"] {
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO service_deployment_events(deployment_id,service_id,phase,status,message,metadata)
             VALUES($1,$1,'previous-retire-wait','running','test historical intent',$2)",
            vec![service.clone().into(),json!({"response":{"previousDeploymentId":previous}}).into()])).await?;
    }
    assert_eq!(rollback(headers,State(state.clone()),Path(service.clone())).await.0,StatusCode::ACCEPTED);
    for _ in 0..4 { tick(&state,&service).await?; }
    assert_eq!(load(Some(db),&service).await?.unwrap().status,"rolled_back");
    assert!(ensure_no_regional_rollout(db,&service).await.is_ok());
    let finished = load(Some(db),&service).await?.unwrap();
    assert_eq!(serde_json::to_value(&finished.plan)?,serde_json::to_value(&plan)?);
    let item = finished.items.as_array().unwrap().iter().find(|i| i["stepId"] == "0:creating:0").unwrap();
    assert_eq!(item["attempts"],2);
    assert_eq!(item["status"],"completed");
    let events = finished.events.as_array().unwrap();
    assert!(events.iter().any(|e| e["status"] == "blocked" && e["stepId"] == "0:creating:0"));
    assert_eq!(events.last().unwrap()["status"],"rolled_back");
    assert!(events.windows(2).all(|p| p[0]["id"].as_i64() < p[1]["id"].as_i64()));
    let restored = snapshot(&service).await?;
    assert!(restored.endpoints.iter().filter(|e|e.deployment_id.starts_with("old-")).all(|e|!e.draining));
    assert!(restored.endpoints.iter().find(|e|e.deployment_id==slots[0].candidate_id).unwrap().draining);
    service_discovery::mark_endpoint_active(&service,"old-eu").await?;
    let cancellations = db.query_all(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT metadata->'response'->>'previousDeploymentId' AS previous FROM service_deployment_events
         WHERE deployment_id=$1 AND phase='previous-retire-cancelled'",
        [service.clone().into()])).await?;
    assert_eq!(cancellations.len(),1,"reactivation must cancel exactly the restored replica's intent, idempotently");
    assert_eq!(cancellations[0].try_get::<String>("","previous")?,"old-eu");

    // Successful asymmetric plan, deliberately reversing regional order. Each
    // tick reloads the durable plan; slot IDs must never be renumbered.
    let operation = format!("{service}-second");
    let second_slots: Vec<_> = ["us","eu","eu"].iter().enumerate().map(|(index,region)| Slot {
        index,region:region.to_string(),candidate_id:candidate_id(&operation,index),
        runtime: None,
    }).collect();
    let mut second_request = req.clone();
    second_request.operation_id = operation.clone();
    second_request.deployment.desired_replicas = Some(3);
    second_request.deployment.replica_regions = vec!["us".into(),"eu".into(),"eu".into()];
    let second_plan = Plan::compile(&["us".into(),"eu".into()], &second_slots.iter()
        .map(|s| (s.region.clone(),s.candidate_id.clone())).collect::<Vec<_>>());
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,
         observer_topology,regions,slots,baseline_state,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
         VALUES($1,$2,$3,$4,'new',$5,$6,$7,$8,$9,1,10,60,'running','preflight')",
        vec![operation.clone().into(),service.clone().into(),payload_hash(&second_request)?.into(),
            serde_json::to_value(&second_request.deployment)?.into(),super::super::regional_observers::topology(&state,&service)?.into(),
            json!(["us","eu"]).into(),serde_json::to_value(&second_slots)?.into(),serde_json::to_value(&baseline)?.into(),serde_json::to_value(&second_plan)?.into()])).await?;
    for _ in 0..40 {
        let current = load(Some(db),&operation).await?.unwrap();
        assert_eq!(current.status,"running","{:?}",current.error);
        if current.phase == "create_slot" {
            let regional: Vec<_> = second_slots.iter().filter(|s|s.region==current.regions[current.region_index]).collect();
            if let Some(slot) = regional.get(current.slot_index) {
                update(db,&current,"creating",current.slot_index,None,None,None).await?;
                service_discovery::publish_healthy_endpoint(&service,&slot.candidate_id,None,
                    Some(&slot.region),Some("new"),&endpoints[3+slot.index],false).await?;
            }
        }
        if current.phase == "bake" {
            db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                "UPDATE regional_service_rollouts SET deadline_at=NOW()-INTERVAL '1 second',last_observed_at=NOW() WHERE operation_id=$1",
                [operation.clone().into()])).await?;
        }
        tick(&state,&operation).await?;
        if load(Some(db),&operation).await?.unwrap().status == "passed" { break; }
    }
    let completed = load(Some(db),&operation).await?.unwrap();
    assert_eq!(completed.status,"passed","{:?}",completed.error);
    assert_eq!(completed.region_index,2);
    for step in &second_plan.steps {
        let item = completed.items.as_array().unwrap().iter().find(|i| i["stepId"] == step.id).unwrap();
        assert_eq!(item["status"],"completed","{}",step.id);
    }
    let final_snapshot = snapshot(&service).await?;
    let serving: Vec<_> = final_snapshot.endpoints.iter().filter(|e|!e.draining).collect();
    assert_eq!(serving.len(),3);
    assert_eq!(serving.iter().filter(|e|e.region.as_deref()==Some("eu")).count(),2);
    assert_eq!(serving.iter().filter(|e|e.region.as_deref()==Some("us")).count(),1);
    assert!(serving.iter().all(|e|e.revision.as_deref()==Some("new")));
    for server in servers { server.abort(); }
    Ok(())
}
