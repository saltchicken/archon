use archon::core::config::ScheduleConfig;
use archon::core::schedule::{
    quote_systemd_arg, render_service, render_timer, sanitize_unit_component, unit_base_name,
};
use std::path::Path;

#[test]
fn sanitize_unit_component_normalizes_names() {
    assert_eq!(sanitize_unit_component("Daily_Audit!"), "daily-audit");
    assert_eq!(sanitize_unit_component("---"), "default");
    assert_eq!(
        unit_base_name("archon", "daily_check"),
        "archon-sched-archon--daily-check"
    );
}

#[test]
fn quote_systemd_arg_escapes_special_chars_and_dollars() {
    assert_eq!(
        quote_systemd_arg("Check $HOME and ${USER} 100% \"now\"\n"),
        "\"Check $$HOME and $${USER} 100%% \\\"now\\\"\\n\""
    );
}

#[test]
fn render_service_and_timer_produce_valid_systemd_units() {
    let sched = ScheduleConfig {
        name: "nightly-review".to_string(),
        enabled: true,
        on_calendar: "*-*-* 02:00:00".to_string(),
        persistent: true,
        skill: Some("review".to_string()),
        prompt: "Review $BRANCH (100%)".to_string(),
        model: Some("gemini-2.5-pro".to_string()),
        system_prompt: None,
        auto_approve: true,
        no_tools: false,
        enabled_tools: Some(vec!["read_file".to_string(), "search".to_string()]),
        save_session: false,
    };

    let service = render_service(
        &sched,
        "archon",
        Path::new("/home/user/project"),
        Path::new("/usr/bin/archon"),
    );
    assert!(service.contains("WorkingDirectory=/home/user/project\n"));
    assert!(!service.contains("WorkingDirectory=\""));
    assert!(service.contains(
        "ExecStart=\"/usr/bin/archon\" --skill \"review\" --model \"gemini-2.5-pro\" -y --allowed-tools \"read_file,search\" --no-save -p \"Review $$BRANCH (100%%)\""
    ));

    let timer = render_timer(
        &sched,
        "archon",
        Path::new("/home/user/project"),
        "archon-sched-archon--nightly-review.service",
    );
    assert!(timer.contains("OnCalendar=*-*-* 02:00:00\n"));
    assert!(timer.contains("Persistent=true\n"));
    assert!(timer.contains("Unit=archon-sched-archon--nightly-review.service\n"));
}
