use super::*;

#[tokio::test]
async fn creating_an_account_stores_registers_and_reports_it() {
    let h = rig("accounts,read", None).await;
    let created = h.create("abc123").await;
    let off = json!({"post": false, "like": false, "follow": false});
    assert_eq!(created["id"], "abc123");
    assert_eq!(created["registered"], true);
    assert_eq!(created["identity"]["userId"], "123");
    assert_eq!(created["identity"]["screenName"], "fixture");
    assert_eq!(created["permissions"], off);
    let revision = created["credentialRevision"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(revision).is_ok());
    let state = created["receiverState"].as_str().unwrap();
    assert!(["connecting", "connected"].contains(&state));

    let saved = h.account("abc123").unwrap();
    assert_eq!(saved.machine_owner.as_deref(), Some(CLIENT));
    assert_eq!(saved.x_user_id.as_deref(), Some("123"));
    assert_eq!(saved.twitter.auth_token, AUTH);
    assert_eq!(saved.webhook_endpoint.as_deref(), Some(HOOK));
    assert_eq!(saved.webhook_secret.as_deref(), Some(HOOK_SECRET));
    assert!(saved.registration.is_some() && !saved.machine_post_enabled);
    let registered = h.backend.registered_with.lock().unwrap().clone();
    assert_eq!(registered, [AUTH]);

    // The owner reads the same facts back; nothing returns a cookie.
    let ready = ok(h.get(READY).await, 200);
    assert_eq!(ready["x_user_id"], "123");
    assert_eq!(ready["permissions"], off);
    assert_eq!(ready["credential_revision"], revision);
    assert_eq!(ready["credentials_configured"], true);
    let listed = ok(h.get(NEW).await, 200);
    assert_eq!(listed["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(listed["accounts"][0]["id"], "abc123");
    // The WebUI's own service treats it as an ordinary registered account.
    let service = h.state.accounts.get("abc123").unwrap();
    let settings = service.settings().unwrap();
    assert!(settings.configured && settings.webhook_secret_set && settings.credentials_locked);

    // Permissions given at creation are applied; the others stay off.
    let mut body = create_body("def456");
    body["permissions"] = json!({"post": true, "like": true});
    let created = ok(h.post(NEW, &body).await, 201);
    let granted = json!({"post": true, "like": true, "follow": false});
    assert_eq!(created["permissions"], granted);
    assert!(h.account("def456").unwrap().machine_like_enabled);
    // Creating the same id again never changes the first one.
    let again = create_body("abc123");
    is(h.post(NEW, &again).await, 409, "account_exists");
    assert_eq!(h.account("abc123").unwrap().twitter.auth_token, AUTH);
}

#[tokio::test]
async fn an_unverified_create_stores_and_registers_nothing() {
    let h = rig("accounts", None).await;
    let before = h.file();
    let create = create_body("abc123");
    let mut other = create_body("abc123");
    other["expectedXUserId"] = json!("999");
    is(h.post(NEW, &other).await, 409, "identity_mismatch");
    for (field, bad) in [
        ("webhookEndpoint", json!("http://example.com/hook")),
        ("webhookEndpoint", json!("ftp://localhost/hook")),
        ("webhookSecret", json!(" ")),
        ("id", json!("Not-Valid")),
        ("authToken", json!("SECRET has a space")),
        ("ct0", json!("SECRET-\u{3042}")),
        ("expectedXUserId", json!("12x")),
        ("extra", json!(AUTH)),
    ] {
        let mut body = create_body("abc123");
        body[field] = bad;
        is(h.post(NEW, &body).await, 400, "invalid_request");
    }
    h.x.reject.store(true, Ordering::SeqCst);
    is(h.post(NEW, &create).await, 422, "credentials_rejected");
    h.x.reject.store(false, Ordering::SeqCst);
    h.x.down.store(true, Ordering::SeqCst);
    is(h.post(NEW, &create).await, 502, "x_unavailable");
    h.x.down.store(false, Ordering::SeqCst);
    h.x.hang.store(true, Ordering::SeqCst);
    is(h.post(NEW, &create).await, 504, "x_timeout");
    assert_eq!(h.file(), before);
    assert_eq!(h.backend.subscribes.load(Ordering::SeqCst), 0);

    // The body must be JSON, and it is size limited.
    let url = format!("{}{PREFIX}/accounts", h.base);
    let plain = h.client.post(url.as_str()).bearer_auth(TOKEN).body("{}");
    assert_eq!(plain.send().await.unwrap().status(), 415);
    let big = json!({"authToken": "a".repeat(20_000), "ct0": CT0});
    let request = h.client.post(url).bearer_auth(TOKEN).json(&big);
    if let Ok(response) = request.send().await {
        assert_eq!(response.status(), 413);
    }
    assert_eq!(h.file(), before);
}

#[tokio::test]
async fn verifying_cookies_reports_the_identity_and_stores_nothing() {
    let h = rig("accounts", None).await;
    let before = h.file();
    let body = json!({"authToken": AUTH, "ct0": CT0});
    let verified = ok(h.post(VERIFY, &body).await, 200);
    assert_eq!(verified["identity"]["userId"], "123");
    assert!(verified["identity"].get("avatarUrl").is_none());
    let sized = "https://pbs.twimg.com/profile_images/1/abc_normal.jpg";
    *h.x.avatar.lock().unwrap() = Some(sized.into());
    let verified = ok(h.post(VERIFY, &body).await, 200);
    let original = "https://pbs.twimg.com/profile_images/1/abc.jpg";
    assert_eq!(verified["identity"]["avatarUrl"], original);

    for bad in [
        json!({"authToken": AUTH}),
        json!({"authToken": AUTH, "ct0": CT0, "expectedXUserId": "123"}),
        json!({"authToken": "SECRET;x", "ct0": CT0}),
    ] {
        is(h.post(VERIFY, &bad).await, 400, "invalid_request");
    }
    h.x.reject.store(true, Ordering::SeqCst);
    is(h.post(VERIFY, &body).await, 422, "credentials_rejected");
    assert_eq!(h.file(), before);
    assert_eq!(h.backend.subscribes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_registration_removes_the_new_account() {
    let h = rig("accounts", None).await;
    let create = create_body("abc123");
    h.backend.fail_twitter.store(true, Ordering::SeqCst);
    is(h.post(NEW, &create).await, 502, "registration_failed");
    assert!(h.account("abc123").is_none());
    assert!(h.state.accounts.get("abc123").is_err());
    // Only the subscription the failed registration made was cleaned up.
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 1);
    // The id is free again.
    h.backend.fail_twitter.store(false, Ordering::SeqCst);
    h.create("abc123").await;
}

#[tokio::test]
async fn a_rollback_is_reported_and_a_failed_one_leaves_the_account_to_delete() {
    let h = rig("accounts", None).await;
    let create = create_body("abc123");
    // Registration works, but the revision cannot be recorded: the create is undone, including
    // the push registration it made.
    h.state.ledger.read_only();
    is(h.post(NEW, &create).await, 503, "storage_unavailable");
    assert!(h.account("abc123").is_none());
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 1);
    // The same failure while the push service refuses to unregister cannot be undone.
    h.backend.fail_unregister.store(true, Ordering::SeqCst);
    is(h.post(NEW, &create).await, 500, "rollback_failed");
    assert!(h.account("abc123").unwrap().registration.is_some());
    // The client's cleanup is a delete, which works once the push service answers again.
    is(h.delete(ACCOUNT).await, 502, "unregistration_failed");
    h.backend.fail_unregister.store(false, Ordering::SeqCst);
    assert_eq!(h.delete(ACCOUNT).await.0, 204);
    assert!(h.account("abc123").is_none());
}

#[tokio::test]
async fn racing_creates_of_one_id_make_one_account() {
    let h = rig("accounts", None).await;
    let body = create_body("abc123");
    let (first, second) = tokio::join!(h.post(NEW, &body), h.post(NEW, &body));
    let mut statuses = [first.0, second.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [201, 409]);
    assert_eq!(h.backend.subscribes.load(Ordering::SeqCst), 1);
    assert!(h.account("abc123").is_some());
}

#[tokio::test]
async fn the_management_ui_still_works_on_a_machine_account() {
    let h = rig("accounts,read", None).await;
    h.create("abc123").await;
    let assets = std::path::Path::new("/nonexistent-fixture-assets");
    let management = crate::web::router(Arc::clone(&h.state.accounts), assets);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, management).await.unwrap();
    });
    let base = format!("http://{address}/api/accounts");
    let listed = h.client.get(base.as_str()).send().await.unwrap();
    let listed = listed.text().await.unwrap();
    assert!(listed.contains("abc123") && !listed.contains("SECRET-"));
    let url = format!("{base}/abc123/settings");
    let settings = h.client.get(url).send().await.unwrap();
    let settings = settings.text().await.unwrap();
    assert!(!settings.contains("SECRET-"), "{settings}");
    let settings: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["configured"], true);
    assert_eq!(settings["webhook_secret_set"], true);
    assert_eq!(settings["credentials_locked"], true);
    assert_eq!(settings["machine_post_enabled"], false);
    // The WebUI deletes it like any other account; the owner then no longer sees it.
    let deleted = h.client.delete(format!("{base}/abc123")).send().await;
    assert_eq!(deleted.unwrap().status(), 204);
    assert!(h.account("abc123").is_none());
    is(h.get(READY).await, 404, "account_not_found");
    task.abort();
}
