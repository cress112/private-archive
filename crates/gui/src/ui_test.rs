//! Test-only driver invokes the same public callbacks as pointer/keyboard input.
use crate::*;
use serde_json::{Value, json};
use std::{cell::Cell, path::PathBuf, time::Instant};
struct Driver {
    actions: Vec<Value>,
    index: Cell<usize>,
    report: PathBuf,
    states: RefCell<Vec<Value>>,
    started: Instant,
    budget: Duration,
}
pub fn start(ui: &AppWindow, args: GuiArgs) {
    let Some(report) = args.test_report else {
        return;
    };
    let mut actions = args
        .test_actions
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok())
        .unwrap_or_default();
    let budget = Duration::from_millis(
        actions
            .first()
            .filter(|a| a["action"] == "budget")
            .and_then(|a| a["ms"].as_u64())
            .unwrap_or(90_000)
            .clamp(1_000, 900_000),
    );
    if actions.first().is_some_and(|a| a["action"] == "budget") {
        actions.remove(0);
    }
    actions.extend([
        json!({"action":"wait_idle"}),
        json!({"action":"snapshot","name":"final"}),
        json!({"action":"close"}),
    ]);
    let driver = Rc::new(Driver {
        actions,
        index: Cell::new(0),
        report,
        states: RefCell::new(vec![]),
        started: Instant::now(),
        budget,
    });
    later(ui.as_weak(), driver, Duration::from_millis(350));
}
fn later(weak: slint::Weak<AppWindow>, driver: Rc<Driver>, duration: Duration) {
    slint::Timer::single_shot(duration, move || step(weak, driver));
}
fn capture(ui: &AppWindow, driver: &Driver, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let image = ui.window().take_snapshot()?;
    let mut ppm = format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
    for p in image.as_slice() {
        ppm.extend_from_slice(&[p.r, p.g, p.b]);
    }
    let path = if name == "final" {
        driver.report.with_extension("ppm")
    } else {
        driver.report.with_file_name(format!(
            "{}-{name}.ppm",
            driver.report.file_stem().unwrap().to_string_lossy()
        ))
    };
    std::fs::write(path, ppm)?;
    let columns:Vec<_>=ui.get_columns().iter().map(|c|{
        let rows:Vec<_>=(0..c.rows.row_count().min(256)).filter_map(|i|c.rows.row_data(i)).filter(|r|r.loaded).map(|r|json!({"name":r.name.as_str(),"uri":r.uri.as_str(),"kind":r.kind.as_str()})).collect();
        json!({"title":c.title.as_str(),"count":c.rows.row_count(),"selected":c.selected,"rows":rows,"width":c.width,"scroll_y":c.scroll_y})
    }).collect();
    let details: Vec<_> = ui
        .get_details()
        .iter()
        .map(|d| json!({"label":d.label.as_str(),"value":d.value.as_str()}))
        .collect();
    let settings:Vec<_>=ui.get_settings().iter().map(|d|json!({"key":d.key.as_str(),"value":d.value.as_str(),"effective":d.effective.as_str(),"origin":d.origin.as_str(),"explicit":d.explicit})).collect();
    let bytes = UI_STATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(0, |s| s.cache.borrow().bytes())
    });
    let value = json!({"name":name,"index":ui.get_index_label().as_str(),"status":ui.get_status().as_str(),"error":ui.get_error_message().as_str(),"rows":columns.first().and_then(|c|c.get("rows")).cloned().unwrap_or(json!([])),"columns":columns,"details":details,"settings":settings,"settings_open":ui.get_settings_open(),"settings_exists":ui.get_settings_exists(),"reconciling":ui.get_reconciling(),"diagnostics":ui.get_diagnostics().as_str(),"row_cache_bytes":bytes,"width":image.width(),"height":image.height(),"window_visible":ui.window().is_visible(),"row_height":ui.get_row_height(),"columns_x":ui.get_columns_x(),"columns_visible_width":ui.get_columns_visible_width(),"columns_content_width":ui.get_columns_content_width(),"can_next":false,"can_previous":false});
    let mut value = value;
    value["elapsed_ms"] = json!(driver.started.elapsed().as_millis());
    value["loading"] = json!(ui.get_loading());
    driver.states.borrow_mut().push(value.clone());
    std::fs::write(&driver.report, serde_json::to_vec_pretty(&value)?)?;
    std::fs::write(
        driver.report.with_extension("states.json"),
        serde_json::to_vec_pretty(&*driver.states.borrow())?,
    )?;
    Ok(())
}
fn step(weak: slint::Weak<AppWindow>, driver: Rc<Driver>) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    if driver.started.elapsed() > driver.budget {
        eprintln!("test driver timeout");
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::CloseRequested);
        return;
    }
    let index = driver.index.get();
    let Some(action) = driver.actions.get(index) else {
        return;
    };
    let kind = action["action"].as_str().unwrap_or("");
    let busy = ui.get_loading() || ui.get_reconciling();
    if matches!(
        kind,
        "edit_setting" | "reset_setting" | "save_settings" | "cancel_settings"
    ) && !ui.get_settings_open()
    {
        later(weak, driver, Duration::from_millis(20));
        return;
    }

    if kind == "wait_file" && !std::path::Path::new(action["path"].as_str().unwrap_or("")).exists()
    {
        later(weak, driver, Duration::from_millis(20));
        return;
    }
    if kind == "select_uri" {
        let column = action["column"].as_i64().unwrap_or(0) as usize;
        if let Some(c) = ui.get_columns().row_data(column) {
            let target = action["uri"].as_str().unwrap_or("");
            let target_kind = action["kind"].as_str();
            let mut found = None;
            let mut loading = false;
            for i in 0..c.rows.row_count().min(4096) {
                if let Some(r) = c.rows.row_data(i) {
                    if !r.loaded {
                        loading = true;
                    } else if r.uri.as_str() == target
                        && target_kind.is_none_or(|v| v == r.kind.as_str())
                    {
                        found = Some(i);
                        break;
                    }
                }
            }
            if let Some(row) = found {
                ui.invoke_select_row(column as i32, row as i32);
                driver.index.set(index + 1);
                later(weak, driver, Duration::from_millis(120));
                return;
            }
            if loading {
                later(weak, driver, Duration::from_millis(30));
                return;
            }
        }
        eprintln!("test selection missing: {action}");
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::CloseRequested);
        return;
    }

    if kind == "snapshot"
        && ui
            .get_columns()
            .iter()
            .any(|c| c.rows.row_count() > 0 && c.rows.row_data(0).is_some_and(|r| !r.loaded))
    {
        later(weak, driver, Duration::from_millis(25));
        return;
    }

    if (kind == "wait_idle" && busy)
        || (ui.get_loading()
            && !matches!(kind, "cancel" | "close" | "wait" | "snapshot")
            && !action["force"].as_bool().unwrap_or(false))
    {
        later(weak, driver, Duration::from_millis(20));
        return;
    }
    let n = |key: &str| action[key].as_i64().unwrap_or(0) as i32;
    match kind {
        "pointer_down" | "pointer_move" | "pointer_up" | "scroll" => {
            let position = slint::LogicalPosition::new(
                action["x"].as_f64().unwrap_or(0.) as f32,
                action["y"].as_f64().unwrap_or(0.) as f32,
            );
            let event = match kind {
                "pointer_down" => {
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
                    slint::platform::WindowEvent::PointerPressed {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    }
                }
                "pointer_up" => slint::platform::WindowEvent::PointerReleased {
                    position,
                    button: slint::platform::PointerEventButton::Left,
                },
                "scroll" => slint::platform::WindowEvent::PointerScrolled {
                    position,
                    delta_x: action["dx"].as_f64().unwrap_or(0.) as f32,
                    delta_y: action["dy"].as_f64().unwrap_or(0.) as f32,
                },
                _ => slint::platform::WindowEvent::PointerMoved { position },
            };
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
            ui.window().dispatch_event(event);
        }
        "click" => {
            let position = slint::LogicalPosition::new(
                action["x"].as_f64().unwrap_or(0.) as f32,
                action["y"].as_f64().unwrap_or(0.) as f32,
            );
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                    position,
                    button: slint::platform::PointerEventButton::Left,
                });
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(35), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                            position,
                            button: slint::platform::PointerEventButton::Left,
                        });
                }
            });
        }
        "text" => {
            for character in action["value"].as_str().unwrap_or("").chars() {
                let text: slint::SharedString = character.to_string().into();
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                        text: text.clone(),
                    });
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
            }
        }
        "select" => ui.invoke_select_row(n("column"), n("row")),
        "top" => ui.invoke_select_top(),
        "reload" => ui.invoke_reload(),
        "reconcile" => {
            ui.set_depth(action["depth"].as_str().unwrap_or("").into());
            ui.invoke_reconcile();
        }
        "cancel" => ui.invoke_cancel_operation(),
        "settings" => ui.invoke_open_settings(),
        "create_settings" => ui.invoke_create_settings(),
        "edit_setting" => {
            ui.invoke_edit_setting(n("index"), action["value"].as_str().unwrap_or("").into())
        }
        "reset_setting" => ui.invoke_reset_setting(n("index")),
        "save_settings" => ui.invoke_save_settings(),
        "cancel_settings" => ui.invoke_close_settings(),
        "sort" => {
            ui.invoke_open_sort();
            if let Some(terms) = action["terms"].as_array() {
                ui.set_sorts(ModelRc::new(VecModel::from(
                    terms
                        .iter()
                        .map(|v| SortData {
                            choice: v.as_i64().unwrap_or(4) as i32,
                        })
                        .collect::<Vec<_>>(),
                )));
            }
            ui.invoke_apply_sort();
        }
        "snapshot" => {
            if let Err(e) = capture(&ui, &driver, action["name"].as_str().unwrap_or("snapshot")) {
                eprintln!("test snapshot: {e}");
            }
        }
        "close" => {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::CloseRequested);
            return;
        }
        _ => {}
    }
    let delay = action["ms"].as_u64().unwrap_or(120);
    driver.index.set(index + 1);
    later(weak, driver, Duration::from_millis(delay));
}
