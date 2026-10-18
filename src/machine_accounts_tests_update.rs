use super::*;

fn fresh(auth: &str, ct0: &str) -> Value {
    json!({"authToken": auth, "ct0": ct0, "expectedXUserId": "123"})
}

#[tokio::test]
async fn new_cookies_are_registered_again_and_rotate_the_revision() {
    let h = rig("accounts,read", None).await;
    let created = h.create("abc123").await;
    let renewed = ok(h.put(CREDS, &fresh(NEXT_AUTH, NEXT_CT0)).await, 200);
    assert_eq!(renewed["registered"], true);
    assert_eq!(renewed["identity"]["userId"], "123");
    assert_ne!(renewed["credentialRevision"], created["credentialRevision"]);

    let saved = h.account("abc123").unwrap();
    assert_eq!(saved.twitter.auth_token, NEXT_AUTH);
    assert_eq!(saved.twitter.ct0, NEXT_CT0);
    assert!(saved.registration.is_some());
    assert_eq!(saved.webhook_endpoint.as_deref(), Some(HOOK));
    assert_eq!(saved.machine_owner.as_deref(), Some(CLIENT));
    // The old registration was removed, then the new cookies registered once.
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 1);
    let registered = h.backend.registered_with.lock().unwrap().clone();
    assert_eq!(registered, [AUTH, NEXT_AUTH]);
    let ready = ok(h.get(READY).await, 200);
    assert_eq!(ready["credential_revision"], renewed["credentialRevision"]);
    assert_eq!(ready["x_user_id"], "123");
    assert_eq!(ready["registered"], true);
    // Exactly one receiver runs afterwards.
    receivers(&h, 1).await;
    assert_eq!(h.backend.max_active.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_same_cookies_change_nothing() {
    let h = rig("accounts,read", None).await;
    let created = h.create("abc123").await;
    let before = h.file();
    let renewed = ok(h.put(CREDS, &fresh(AUTH, CT0)).await, 200);
    assert_eq!(renewed["credentialRevision"], created["credentialRevision"]);
    assert_eq!(renewed["registered"], true);
    assert_eq!(h.file(), before);
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 0);
    assert_eq!(h.backend.subscribes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cookies_of_another_person_are_refused() {
    let h = rig("accounts,read", None).await;
    h.create("abc123").await;
    let before = h.file();
    // The new cookies belong to 999, whatever the client expected.
    *h.x.user.lock().unwrap() = Some("999".into());
    for expected in ["123", "999"] {
        let mut body = fresh(NEXT_AUTH, NEXT_CT0);
        body["expectedXUserId"] = json!(expected);
        is(h.put(CREDS, &body).await, 409, "identity_mismatch");
    }
    // They belong to the saved person, but the client expected someone else.
    *h.x.user.lock().unwrap() = None;
    let mut body = fresh(NEXT_AUTH, NEXT_CT0);
    body["expectedXUserId"] = json!("555");
    is(h.put(CREDS, &body).await, 409, "identity_mismatch");
    assert_eq!(h.file(), before);
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 0);
    assert_eq!(h.backend.subscribes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bad_requests_and_x_failures_leave_the_account_untouched() {
    let h = rig("accounts,read", None).await;
    h.create("abc123").await;
    let before = h.file();
    let good = fresh(NEXT_AUTH, NEXT_CT0);
    for (field, bad) in [
        ("authToken", json!("SECRET;x")),
        ("ct0", json!("")),
        ("expectedXUserId", json!("abc")),
        ("extra", json!(NEXT_AUTH)),
    ] {
        let mut body = good.clone();
        body[field] = bad;
        is(h.put(CREDS, &body).await, 400, "invalid_request");
    }
    h.x.reject.store(true, Ordering::SeqCst);
    is(h.put(CREDS, &good).await, 422, "credentials_rejected");
    h.x.reject.store(false, Ordering::SeqCst);
    h.x.down.store(true, Ordering::SeqCst);
    is(h.put(CREDS, &good).await, 502, "x_unavailable");
    h.x.down.store(false, Ordering::SeqCst);
    h.x.hang.store(true, Ordering::SeqCst);
    is(h.put(CREDS, &good).await, 504, "x_timeout");
    assert_eq!(h.file(), before);
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 0);
    // The hung request released the account lock when it timed out.
    h.x.hang.store(false, Ordering::SeqCst);
    ok(h.put(CREDS, &good).await, 200);
}

#[tokio::test]
async fn a_failed_reregistration_keeps_the_new_cookies_and_a_retry_finishes_it() {
    let h = rig("accounts,read", None).await;
    let created = h.create("abc123").await;
    let body = fresh(NEXT_AUTH, NEXT_CT0);
    h.backend.fail_twitter.store(true, Ordering::SeqCst);
    is(h.put(CREDS, &body).await, 502, "registration_failed");
    // The account is not rolled back: new cookies saved, nothing registered.
    let saved = h.account("abc123").unwrap();
    assert_eq!(saved.twitter.auth_token, NEXT_AUTH);
    assert!(saved.registration.is_none());
    let ready = ok(h.get(READY).await, 200);
    assert_eq!(ready["registered"], false);
    assert_ne!(ready["credential_revision"], created["credentialRevision"]);
    // The same request again registers it, without a second cookie change.
    h.backend.fail_twitter.store(false, Ordering::SeqCst);
    let renewed = ok(h.put(CREDS, &body).await, 200);
    assert_eq!(renewed["registered"], true);
    assert_eq!(renewed["credentialRevision"], ready["credential_revision"]);
    assert!(h.account("abc123").unwrap().registration.is_some());
    // Old subscription removed once, the failed new one cleaned up once.
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_failed_unregistration_keeps_the_old_cookies_and_receiver() {
    let h = rig("accounts,read", None).await;
    let created = h.create("abc123").await;
    h.backend.fail_unregister.store(true, Ordering::SeqCst);
    let body = fresh(NEXT_AUTH, NEXT_CT0);
    is(h.put(CREDS, &body).await, 502, "registration_failed");
    let saved = h.account("abc123").unwrap();
    assert_eq!(saved.twitter.auth_token, AUTH);
    assert!(saved.registration.is_some());
    let ready = ok(h.get(READY).await, 200);
    assert_eq!(ready["credential_revision"], created["credentialRevision"]);
    receivers(&h, 1).await;
    // Once the push service answers again the same request goes through.
    h.backend.fail_unregister.store(false, Ordering::SeqCst);
    ok(h.put(CREDS, &body).await, 200);
    assert_eq!(h.account("abc123").unwrap().twitter.auth_token, NEXT_AUTH);
}

#[tokio::test]
async fn the_webhook_changes_without_touching_cookies_or_the_revision() {
    let h = rig("accounts,read", None).await;
    let created = h.create("abc123").await;
    let before = h.account("abc123").unwrap();
    let local = "http://127.0.0.1:9/hook";
    let body = json!({"endpoint": local, "secret": "SECRET-NEW-HOOK"});
    assert_eq!(ok(h.put(HOOKS, &body).await, 200), json!({"updated": true}));
    let saved = h.account("abc123").unwrap();
    assert_eq!(saved.webhook_endpoint.as_deref(), Some(local));
    assert_eq!(saved.webhook_secret.as_deref(), Some("SECRET-NEW-HOOK"));
    assert_eq!(saved.twitter.auth_token, before.twitter.auth_token);
    assert_eq!(saved.twitter.ct0, before.twitter.ct0);
    assert_eq!(saved.credential_revision, before.credential_revision);
    assert!(saved.registration.is_some());
    let ready = ok(h.get(READY).await, 200);
    assert_eq!(ready["credential_revision"], created["credentialRevision"]);
    assert_eq!(h.backend.subscribes.load(Ordering::SeqCst), 1);
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 0);

    let after = h.file();
    let too_long = "x".repeat(MAX_SECRET_LEN + 1);
    for (field, bad) in [
        ("endpoint", json!("http://example.com/hook")),
        ("endpoint", json!("")),
        ("endpoint", json!(format!(" {HOOK}"))),
        ("endpoint", json!("ftp://127.0.0.1/hook")),
        ("secret", json!("  ")),
        ("secret", json!(too_long)),
        ("secret", json!("line\nbreak")),
        ("extra", json!(HOOK_SECRET)),
    ] {
        let mut request = json!({"endpoint": HOOK, "secret": HOOK_SECRET});
        request[field] = bad;
        is(h.put(HOOKS, &request).await, 400, "invalid_request");
    }
    let partial = json!({"endpoint": HOOK});
    is(h.put(HOOKS, &partial).await, 400, "invalid_request");
    assert_eq!(h.file(), after);
}

#[tokio::test]
async fn permissions_merge_and_never_touch_the_revision() {
    let h = rig("accounts,read", None).await;
    let created = h.create("abc123").await;
    let reply = ok(h.put(PERMS, &json!({"like": true})).await, 200);
    let granted = json!({"post": false, "like": true, "follow": false});
    assert_eq!(reply, json!({"permissions": granted}));
    let reply = ok(h.put(PERMS, &json!({"follow": true})).await, 200);
    let granted = json!({"post": false, "like": true, "follow": true});
    assert_eq!(reply["permissions"], granted);
    // An empty body keeps everything.
    let reply = ok(h.put(PERMS, &json!({})).await, 200);
    assert_eq!(reply["permissions"], granted);
    let mixed = json!({"post": true, "like": false});
    let reply = ok(h.put(PERMS, &mixed).await, 200);
    let granted = json!({"post": true, "like": false, "follow": true});
    assert_eq!(reply["permissions"], granted);

    let saved = h.account("abc123").unwrap();
    assert!(saved.machine_post_enabled && saved.machine_follow_enabled);
    assert!(!saved.machine_like_enabled);
    let ready = ok(h.get(READY).await, 200);
    assert_eq!(ready["permissions"], granted);
    assert_eq!(ready["credential_revision"], created["credentialRevision"]);
    // The WebUI service reads the same switches.
    let settings = h.state.accounts.get("abc123").unwrap().settings().unwrap();
    assert!(settings.machine_post_enabled && !settings.machine_like_enabled);

    let before = h.file();
    for bad in [
        json!({"other": true}),
        json!({"post": "yes"}),
        json!([true]),
        json!([true, false, false]),
    ] {
        is(h.put(PERMS, &bad).await, 400, "invalid_request");
    }
    assert_eq!(h.file(), before);
}

#[tokio::test]
async fn deleting_stops_the_receiver_and_keeps_the_ledger() {
    let h = rig("accounts,read", None).await;
    h.create("abc123").await;
    receivers(&h, 1).await;
    let (record, _) = h
        .state
        .ledger
        .reserve("abc123", CLIENT, "key", "hash", "revision")
        .unwrap();

    assert_eq!(h.delete(ACCOUNT).await.0, 204);
    assert!(h.account("abc123").is_none());
    assert_eq!(h.backend.unregisters.load(Ordering::SeqCst), 1);
    receivers(&h, 0).await;
    assert!(h.state.accounts.get("abc123").is_err());
    let kept = h.state.ledger.get("abc123", CLIENT, &record.operation_id);
    assert!(kept.unwrap().is_some());
    // It is gone for the client too, and the id can be used again.
    is(h.delete(ACCOUNT).await, 404, "account_not_found");
    is(h.get(READY).await, 404, "account_not_found");
    h.create("abc123").await;
    let kept = h.state.ledger.get("abc123", CLIENT, &record.operation_id);
    assert!(kept.unwrap().is_some());
}

#[tokio::test]
async fn operations_on_one_account_wait_for_each_other() {
    let h = rig("accounts,read", None).await;
    h.create("abc123").await;
    let credentials = fresh(NEXT_AUTH, NEXT_CT0);
    let hook = json!({"endpoint": HOOK, "secret": HOOK_SECRET});
    let permissions = json!({"post": true});
    let (first, second, third) = tokio::join!(
        h.put(CREDS, &credentials),
        h.put(HOOKS, &hook),
        h.put(PERMS, &permissions)
    );
    assert_eq!((first.0, second.0, third.0), (200, 200, 200));
    let saved = h.account("abc123").unwrap();
    assert_eq!(saved.twitter.auth_token, NEXT_AUTH);
    assert!(saved.machine_post_enabled && saved.registration.is_some());
    assert_eq!(h.backend.max_active.load(Ordering::SeqCst), 1);
}
