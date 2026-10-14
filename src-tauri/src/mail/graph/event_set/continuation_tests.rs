use super::*;
use tests::server as client;

#[tokio::test]
async fn pagination_accepts_odata_keys_and_equivalent_percent_encoding_without_rewriting_url() {
    for next in [
        "ROOT/me/calendars('cal%2fkey%2b%3d')/events?$skiptoken=opaque%2Btoken%3D&$top=100",
        "ROOT/me/calendars/cal%2fkey+%3d/events?$skiptoken=opaque%2Btoken%3D&$top=100",
    ] {
        let (client, captured) = client(|_| {
            vec![
                (
                    200,
                    json!({"value":[{"id":"first"}], "@odata.nextLink":next}),
                ),
                (200, json!({"value":[{"id":"second"}]})),
            ]
        })
        .await;
        let path = collection_path("cal/key+=").unwrap();
        let page = client
            .calendar_set_request(Method::GET, &path, &[], None, None)
            .await
            .unwrap();
        let items = client.set_pages(page, &path).await.unwrap();
        assert_eq!(items.len(), 2);
        let requests = captured.await.unwrap();
        let next_path = next.strip_prefix("ROOT").unwrap();
        assert!(
            requests[1].starts_with(&format!("GET /graph{next_path} HTTP/1.1")),
            "{}",
            requests[1]
        );
        assert!(requests.iter().all(|request| request
            .contains("authorization: Bearer secret-test-token")
            && request.contains("IdType=\"ImmutableId\"")));
    }
}

#[tokio::test]
async fn user_alias_is_verified_against_me_and_cached_for_all_pages() {
    for user in [
        "users('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa')",
        "users('AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA')",
        "users/OWNER%40example.test",
    ] {
        let (client, captured) = client(|_| vec![
            (200, json!({"value":[], "@odata.nextLink":format!("ROOT/{user}/calendars('selected')/events?$skiptoken=one")})),
            (200, json!({"id":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "userPrincipalName":"owner@example.test"})),
            (200, json!({"value":[{"id":"one"}], "@odata.nextLink":format!("ROOT/{user}/calendars/selected/events?$skiptoken=two")})),
            (200, json!({"value":[{"id":"two"}]})),
        ]).await;
        let path = "/me/calendars/selected/events";
        let page = client
            .calendar_set_request(Method::GET, path, &[], None, None)
            .await
            .unwrap();
        assert_eq!(client.set_pages(page, path).await.unwrap().len(), 2);
        let requests = captured.await.unwrap();
        assert!(requests[1].starts_with("GET /graph/me?%24select=id%2CuserPrincipalName "));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("GET /graph/me?"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn default_calendar_alias_requires_verified_selected_calendar_id() {
    for alias in ["ROOT/me/calendar/events", "ROOT/me/events"] {
        for matches in [true, false] {
            let (client, captured) = client(|_| {
                let mut pages = vec![
                    (
                        200,
                        json!({"value":[], "@odata.nextLink":format!("{alias}?$skiptoken=opaque")}),
                    ),
                    (
                        200,
                        json!({"id":if matches { "selected" } else { "other" }}),
                    ),
                ];
                if matches {
                    pages.push((200, json!({"value":[{"id":"continued"}]})));
                }
                pages
            })
            .await;
            let path = "/me/calendars/selected/events";
            let page = client
                .calendar_set_request(Method::GET, path, &[], None, None)
                .await
                .unwrap();
            let result = client.set_pages(page, path).await;
            if matches {
                assert_eq!(result.unwrap().len(), 1);
            } else {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("default calendar identity mismatch"));
            }
            let requests = captured.await.unwrap();
            assert!(requests[1].starts_with("GET /graph/me/calendar?%24select=id "));
            assert_eq!(requests.len(), if matches { 3 } else { 2 });
        }
    }
}

#[tokio::test]
async fn immutable_series_navigation_may_omit_calendar_only_for_same_series_and_collection() {
    for collection in ["instances", "exceptionOccurrences"] {
        let (client, captured) = client(|_| vec![
            (200, json!({"value":[], "@odata.nextLink":format!("ROOT/me/events('series%2b%3d')/{collection}?$skiptoken=one")})),
            (200, json!({"value":[{"id":"child"}]})),
        ]).await;
        let scope = format!("/me/calendars/selected/events/series%2B%3D/{collection}");
        let page = client
            .calendar_set_request(Method::GET, &scope, &[], None, None)
            .await
            .unwrap();
        assert_eq!(client.set_pages(page, &scope).await.unwrap().len(), 1);
        assert_eq!(captured.await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn scope_mismatches_are_rejected_before_following_continuation_and_diagnostics_are_redacted()
{
    for (scope, next) in [
        (
            "/me/calendars/selected/events",
            "ROOT/me/calendars('private-other-calendar')/events",
        ),
        (
            "/me/calendars/selected/events/series/exceptionOccurrences",
            "ROOT/me/events('private-other-series')/exceptionOccurrences",
        ),
        (
            "/me/calendars/selected/events/series/exceptionOccurrences",
            "ROOT/me/events('series')/instances",
        ),
        (
            "/me/calendars/selected/events/series/exceptionOccurrences",
            "ROOT/me/calendars('private-other-calendar')/events('series')/exceptionOccurrences",
        ),
        ("/me/calendars/selected/events", "ROOT/me/messages"),
        (
            "/me/calendars/selected/events",
            "ROOT/me/calendars('selected')/events/private-extra",
        ),
        (
            "/me/calendars/cal%2Fkey/events",
            "ROOT/me/calendars('cal%252Fkey')/events",
        ),
    ] {
        let (client, captured) = client(|_| {
            vec![(
                200,
                json!({"value":[], "@odata.nextLink":format!("{next}?$skiptoken=private-token")}),
            )]
        })
        .await;
        let page = client
            .calendar_set_request(Method::GET, scope, &[], None, None)
            .await
            .unwrap();
        let error = client.set_pages(page, scope).await.unwrap_err().to_string();
        assert!(
            error.contains("continuation left selected calendar collection"),
            "{error}"
        );
        assert!(!error.contains("private-") && !error.contains("skiptoken"));
        assert_eq!(captured.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn another_users_collection_is_rejected_after_verifying_me_without_requesting_the_foreign_mailbox(
) {
    let (client, captured) = client(|_| vec![
        (200, json!({"value":[], "@odata.nextLink":"ROOT/users('private-foreign-user')/calendars('selected')/events?$skiptoken=private-token"})),
        (200, json!({"id":"signed-in-user", "userPrincipalName":"owner@example.test"})),
    ]).await;
    let scope = "/me/calendars/selected/events";
    let page = client
        .calendar_set_request(Method::GET, scope, &[], None, None)
        .await
        .unwrap();
    let error = client.set_pages(page, scope).await.unwrap_err().to_string();
    assert!(error.contains("signed-in-user identity mismatch"));
    assert!(!error.contains("private-") && !error.contains("owner@example.test"));
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| !request.contains("private-foreign-user")));
}

#[tokio::test]
async fn continuation_origin_api_version_userinfo_and_fragment_checks_remain_strict() {
    for next in [
        "https://attacker.invalid/me/calendars/selected/events",
        "ROOT/../beta/me/calendars/selected/events",
        "ROOT/me/calendars/selected/events#fragment",
        "ROOT/me/calendars/selected/events/../..",
    ] {
        let (client, captured) =
            client(|_| vec![(200, json!({"value":[], "@odata.nextLink":next}))]).await;
        let scope = "/me/calendars/selected/events";
        let page = client
            .calendar_set_request(Method::GET, scope, &[], None, None)
            .await
            .unwrap();
        assert!(client.set_pages(page, scope).await.is_err(), "{next}");
        assert_eq!(captured.await.unwrap().len(), 1);
    }
    let (client, captured) = client(|root| vec![(200, json!({"value":[], "@odata.nextLink":format!("{}/me/calendars/selected/events", root.replacen("http://", "http://private:secret@", 1))}))]).await;
    let scope = "/me/calendars/selected/events";
    let page = client
        .calendar_set_request(Method::GET, scope, &[], None, None)
        .await
        .unwrap();
    let error = client.set_pages(page, scope).await.unwrap_err().to_string();
    assert!(error.contains("untrusted") && !error.contains("secret"));
    assert_eq!(captured.await.unwrap().len(), 1);
}

#[tokio::test]
async fn cycles_and_conflicting_duplicate_ids_still_fail_instead_of_claiming_complete_inventory() {
    for conflict in [true, false] {
        let (client, captured) = client(|_| vec![
            (200, json!({"value":[{"id":"same", "value":1}], "@odata.nextLink":"ROOT/me/calendars('selected')/events?$skiptoken=next"})),
            (200, if conflict { json!({"value":[{"id":"same", "value":2}]}) } else { json!({"value":[], "@odata.nextLink":"ROOT/me/calendars('selected')/events?$skiptoken=next"}) }),
        ]).await;
        let scope = "/me/calendars/selected/events";
        let page = client
            .calendar_set_request(Method::GET, scope, &[], None, None)
            .await
            .unwrap();
        let error = client.set_pages(page, scope).await.unwrap_err().to_string();
        assert!(error.contains(if conflict {
            "conflicting duplicate"
        } else {
            "pagination link"
        }));
        assert_eq!(captured.await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn unavailable_identity_proof_does_not_allow_continuation() {
    for next in [
        "ROOT/users('owner')/calendars('selected')/events",
        "ROOT/me/calendar/events",
    ] {
        for (status, proof) in [(403, json!({"error":"denied"})), (200, json!({}))] {
            let (client, captured) = client(|_| {
                vec![
                    (200, json!({"value":[], "@odata.nextLink":next})),
                    (status, proof),
                ]
            })
            .await;
            let scope = "/me/calendars/selected/events";
            let page = client
                .calendar_set_request(Method::GET, scope, &[], None, None)
                .await
                .unwrap();
            assert!(client.set_pages(page, scope).await.is_err());
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests[1].starts_with("GET /graph/me?")
                    || requests[1].starts_with("GET /graph/me/calendar?")
            );
        }
    }
}
