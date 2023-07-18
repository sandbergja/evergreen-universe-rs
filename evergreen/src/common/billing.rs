use crate::common::penalty;
use crate::constants as C;
use crate::date;
use crate::editor::Editor;
use crate::settings::Settings;
use crate::util;
use crate::util::{json_bool, json_float, json_int};
use chrono::{DateTime, Duration, Local};
use chrono::prelude::Datelike;
use json::JsonValue;
use std::cmp::Ordering;
use std::collections::HashSet;

const DAY_OF_SECONDS: i64 = 86400;

/// Void a list of billings.
pub fn void_bills(
    editor: &mut Editor,
    billing_ids: &[i64], // money.billing.id
    maybe_note: Option<&str>,
) -> Result<(), String> {
    editor.has_requestor()?;

    let mut bills = editor.search("mb", json::object! {"id": billing_ids})?;
    let mut penalty_users: HashSet<(i64, i64)> = HashSet::new();

    if bills.len() == 0 {
        return Err(format!("No such billings: {billing_ids:?}"));
    }

    for mut bill in bills.drain(0..) {
        if json_bool(&bill["voided"]) {
            log::debug!("Billing {} already voided.  Skipping", bill["id"]);
            continue;
        }

        let xact = editor.retrieve("mbt", bill["xact"].clone())?;
        let xact = match xact {
            Some(x) => x,
            None => return editor.die_event(),
        };

        let xact_org = xact_org(editor, json_int(&xact["id"])?)?;
        let xact_user = json_int(&xact["usr"])?;
        let xact_id = json_int(&xact["id"])?;

        penalty_users.insert((xact_user, xact_org));

        bill["voided"] = json::from("t");
        bill["voider"] = json::from(editor.requestor_id());
        bill["void_time"] = json::from("now");

        if let Some(orig_note) = bill["note"].as_str() {
            if let Some(new_note) = maybe_note {
                bill["note"] = json::from(format!("{}\n{}", orig_note, new_note).as_str());
            }
        } else if let Some(new_note) = maybe_note {
            bill["note"] = json::from(new_note);
        }

        editor.update(&bill)?;
        check_open_xact(editor, xact_id)?;
    }

    for (user_id, org_id) in penalty_users.iter() {
        penalty::calculate_penalties(editor, *user_id, *org_id, None)?;
    }

    Ok(())
}

/// Sets or clears xact_finish on a transaction as needed.
pub fn check_open_xact(editor: &mut Editor, xact_id: i64) -> Result<(), String> {
    let mut xact = match editor.retrieve("mbt", xact_id)? {
        Some(x) => x,
        None => return editor.die_event(),
    };

    let mbts = match editor.retrieve("mbts", xact_id)? {
        Some(m) => m,
        None => return editor.die_event(),
    };

    // See if we have a completed circ.
    let no_circ_or_complete = match editor.retrieve("circ", xact_id)? {
        Some(c) => c["stop_fines"].is_string(), // otherwise is_null()
        None => true,
    };

    let zero_owed = json_float(&mbts["balance_owed"])? == 0.0;
    let xact_open = xact["xact_finish"].is_null();

    if zero_owed {
        if xact_open && no_circ_or_complete {
            // If nothing is owed on the transaction, but it is still open,
            // and this transaction is not an open circulation, close it.

            log::info!("Closing completed transaction {xact_id} on zero balance");
            xact["xact_finish"] = json::from("now");
            return editor.update(&xact);
        }
    } else if !xact_open {
        // Transaction closed but money or refund still owed.

        if !zero_owed && !xact_open {
            log::info!("Re-opening transaction {xact_id} on non-zero balance");
            xact["xact_finish"] = json::JsonValue::Null;
            return editor.update(&xact);
        }
    }

    Ok(())
}

/// Returns the context org unit ID for a transaction (by ID).
pub fn xact_org(editor: &mut Editor, xact_id: i64) -> Result<i64, String> {
    // There's a view for that!
    // money.billable_xact_summary_location_view
    if let Some(sum) = editor.retrieve("mbtslv", xact_id)? {
        json_int(&sum["billing_location"])
    } else {
        Err(format!("No Such Transaction: {xact_id}"))
    }
}

/// Creates and returns a newly created money.billing.
pub fn create_bill(
    editor: &mut Editor,
    amount: f64,
    btype_id: i64,
    btype_label: &str,
    xact_id: i64,
    maybe_note: Option<&str>,
    period_start: Option<&str>,
    period_end: Option<&str>,
) -> Result<JsonValue, String> {
    log::info!("System is charging ${amount} [btype={btype_id}:{btype_label}] on xact {xact_id}");

    let note = maybe_note.unwrap_or("SYSTEM GENERATED");

    let bill = json::object! {
        "xact": xact_id,
        "amount": amount,
        "period_start": period_start,
        "period_end": period_end,
        "billing_type": btype_label,
        "btype": btype_id,
        "note": note,
    };

    let bill = editor.idl().create_from("mb", bill)?;
    editor.create(&bill)
}

/// Void a set of bills (by type) for a transaction or apply
/// adjustments to zero the bills, depending on settings, etc.
pub fn void_or_zero_bills_of_type(
    editor: &mut Editor,
    xact_id: i64,
    context_org: i64,
    btype_id: i64,
    for_note: &str,
) -> Result<(), String> {
    log::info!("Void/Zero Bills for xact={xact_id} and btype={btype_id}");

    let mut settings = Settings::new(&editor);
    let query = json::object! {"xact": xact_id, "btype": btype_id};
    let bills = editor.search("mb", query)?;

    if bills.len() == 0 {
        return Ok(());
    }

    let bill_ids: Vec<i64> = bills
        .iter()
        .map(|b| json_int(&b["id"]).expect("Billing has invalid id?"))
        .collect();

    // "lost" settings are checked first for backwards compat /
    // consistency with Perl.
    let prohibit_neg_balance = json_bool(
        settings.get_value_at_org("bill.prohibit_negative_balance_on_lost", context_org)?,
    ) || json_bool(
        settings.get_value_at_org("bill.prohibit_negative_balance_default", context_org)?,
    );

    let mut neg_balance_interval =
        settings.get_value_at_org("bill.negative_balance_interval_on_lost", context_org)?;

    if neg_balance_interval.is_null() {
        neg_balance_interval =
            settings.get_value_at_org("bill.negative_balance_interval_default", context_org)?;
    }

    let mut has_refundable = false;
    if let Some(interval) = neg_balance_interval.as_str() {
        has_refundable = xact_has_payment_within(editor, xact_id, interval)?;
    }

    if prohibit_neg_balance && !has_refundable {
        let note = format!("System: ADJUSTED {for_note}");
        adjust_bills_to_zero(editor, bill_ids.as_slice(), &note)
    } else {
        let note = format!("System: VOIDED {for_note}");
        void_bills(editor, bill_ids.as_slice(), Some(&note))
    }
}

/// Assumes all bills are linked to the same transaction.
pub fn adjust_bills_to_zero(
    editor: &mut Editor,
    bill_ids: &[i64],
    note: &str,
) -> Result<(), String> {
    let mut bills = editor.search("mb", json::object! {"id": bill_ids})?;
    if bills.len() == 0 {
        return Ok(());
    }

    let xact_id = json_int(&bills[0]["xact"])?;

    let flesh = json::object! {
        "flesh": 2,
        "flesh_fields": {
            "mbt": ["grocery", "circulation"],
            "circ": ["target_copy"]
        }
    };

    let mbt = editor
        .retrieve_with_ops("mbt", xact_id, flesh)?
        .expect("Billing has no transaction?");

    let user_id = json_int(&mbt["usr"])?;
    let mut bill_maps = bill_payment_map_for_xact(editor, xact_id)?;

    let xact_total = match bill_maps
        .iter()
        .map(|m| json_float(&m.bill["amount"]).unwrap())
        .reduce(|a, b| a + b)
    {
        Some(t) => t,
        None => return Ok(()), // should never happen
    };

    for bill in bills.iter_mut() {
        let map = match bill_maps
            .iter_mut()
            .filter(|m| m.bill["id"] == bill["id"])
            .next()
        {
            Some(m) => m,
            None => continue, // should never happen
        };

        // The amount to adjust is the non-adjusted balance on the
        // bill. It should never be less than zero.
        let mut amount_to_adjust = util::fpdiff(map.bill_amount, map.adjustment_amount);

        // Check if this bill is already adjusted.  We don't allow
        // "double" adjustments regardless of settings.
        if amount_to_adjust <= 0.0 {
            continue;
        }

        if amount_to_adjust > xact_total {
            amount_to_adjust = xact_total;
        }

        // Create the account adjustment
        let payment = json::object! {
            "amount": amount_to_adjust,
            "amount_collected": amount_to_adjust,
            "xact": xact_id,
            "accepting_usr": editor.requestor_id(),
            "payment_ts": "now",
            "billing": bill["id"].clone(),
            "note": note,
        };

        let payment = editor.idl().create_from("maa", payment)?;
        editor.create(&payment)?;

        // Adjust our bill_payment_map
        map.adjustment_amount += amount_to_adjust;
        map.adjustments.push(payment);

        // Should come to zero:
        let new_bill_amount = util::fpdiff(json_float(&bill["amount"])?, amount_to_adjust);
        bill["amount"] = json::from(new_bill_amount);
    }

    check_open_xact(editor, xact_id)?;

    let org_id = xact_org(editor, xact_id)?;
    penalty::calculate_penalties(editor, user_id, org_id, None)?;

    Ok(())
}

pub struct BillPaymentMap {
    /// The adjusted bill object
    pub bill: JsonValue,
    /// List of account adjustments that apply directly to the bill.
    pub adjustments: Vec<JsonValue>,
    /// List of payment objects applied to the bill
    pub payments: Vec<JsonValue>,
    /// original amount from the billing object
    pub bill_amount: f64,
    /// Total of account adjustments that apply to the bill.
    pub adjustment_amount: f64,
}

pub fn bill_payment_map_for_xact(
    editor: &mut Editor,
    xact_id: i64,
) -> Result<Vec<BillPaymentMap>, String> {
    let query = json::object! {
        "xact": xact_id,
        "voided": "f",
    };
    let ops = json::object! {
        "order_by": {
            "mb": {
                "billing_ts": {
                    "direction": "asc"
                }
            }
        }
    };

    let mut bills = editor.search_with_ops("mb", query, ops)?;

    let mut maps = Vec::new();

    if bills.len() == 0 {
        return Ok(maps);
    }

    for bill in bills.drain(0..) {
        let amount = json_float(&bill["amount"])?;

        let map = BillPaymentMap {
            bill: bill,
            adjustments: Vec::new(),
            payments: Vec::new(),
            bill_amount: amount,
            adjustment_amount: 0.00,
        };

        maps.push(map);
    }

    let query = json::object! {"xact": xact_id, "voided": "f"};

    let ops = json::object! {
        "flesh": 1,
        "flesh_fields": {"mp": ["account_adjustment"]},
        "order_by": {"mp": {"payment_ts": {"direction": "asc"}}},
    };

    let mut payments = editor.search_with_ops("mp", query, ops)?;

    if payments.len() == 0 {
        // If we have no payments, return the unmodified maps.
        return Ok(maps);
    }

    // Sort payments largest to lowest amount.
    // This will come in handy later.
    payments.sort_by(|a, b| {
        if json_int(&b["amount"]).unwrap() < json_int(&a["amount"]).unwrap() {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    });

    let mut used_adjustments: HashSet<i64> = HashSet::new();

    for map in maps.iter_mut() {
        let bill = &mut map.bill;

        // Find adjustments that apply to this individual billing and
        // has not already been accounted for.
        let mut my_adjustments: Vec<&mut JsonValue> = payments
            .iter_mut()
            .filter(|p| p["payment_type"].as_str().unwrap() == "account_adjustment")
            .filter(|p| {
                used_adjustments.contains(&json_int(&p["account_adjustment"]["id"]).unwrap())
            })
            .filter(|p| p["account_adjustment"]["billing"] == bill["id"])
            .map(|p| &mut p["account_adjustment"])
            .collect();

        if my_adjustments.len() == 0 {
            continue;
        }

        for adjustment in my_adjustments.drain(0..) {
            let adjust_amount = json_float(&adjustment["amount"])?;
            let adjust_id = json_int(&adjustment["id"])?;

            let new_amount = util::fpdiff(json_float(&bill["amount"])?, adjust_amount);

            if new_amount >= 0.0 {
                map.adjustments.push(adjustment.clone());
                map.adjustment_amount += adjust_amount;
                bill["amount"] = json::from(new_amount);
                used_adjustments.insert(adjust_id);
            } else {
                // It should never happen that we have more adjustment
                // payments on a single bill than the amount of the bill.

                // Clone the adjustment to say how much of it actually
                // applied to this bill.
                let mut new_adjustment = adjustment.clone();
                new_adjustment["amount"] = bill["amount"].clone();
                new_adjustment["amount_collected"] = bill["amount"].clone();
                map.adjustments.push(new_adjustment.clone());
                map.adjustment_amount += json_float(&new_adjustment["amount"])?;
                bill["amount"] = json::from(0.0);
                adjustment["amount"] = json::from(-new_amount);
            }

            if json_float(&bill["amount"])? == 0.0 {
                break;
            }
        }
    }

    // Try to map payments to bills by amounts starting with the
    // largest payments.
    let mut used_payments: HashSet<i64> = HashSet::new();
    for payment in payments.iter() {
        let map = match maps
            .iter_mut()
            .filter(|m| {
                m.bill["amount"] == payment["amount"]
                    && !used_payments.contains(&json_int(&payment["id"]).unwrap())
            })
            .next()
        {
            Some(m) => m,
            None => continue,
        };

        map.bill["amount"] = json::from(0.0);
        map.payments.push(payment.clone());
        used_payments.insert(json_int(&payment["id"])?);
    }

    // Remove the used payments from our working list.
    let mut new_payments = Vec::new();
    for pay in payments.drain(0..) {
        if !used_payments.contains(&json_int(&pay["id"])?) {
            new_payments.push(pay);
        }
    }
    payments = new_payments;
    let mut used_payments = HashSet::new();

    // Map remaining bills to payments in whatever order.
    for map in maps
        .iter_mut()
        .filter(|m| json_float(&m.bill["amount"]).unwrap() > 0.0)
    {
        let bill = &mut map.bill;
        // Loop over remaining unused / unmapped payments.
        for pay in payments
            .iter_mut()
            .filter(|p| !used_payments.contains(&json_int(&p["id"]).unwrap()))
        {
            loop {
                let bill_amount = json_float(&bill["amount"])?;
                if bill_amount > 0.0 {
                    let new_amount = util::fpdiff(bill_amount, json_float(&pay["amount"])?);
                    if new_amount < 0.0 {
                        let mut new_payment = pay.clone();
                        new_payment["amount"] = json::from(bill_amount);
                        bill["amount"] = json::from(0.0);
                        map.payments.push(new_payment);
                        pay["amount"] = json::from(-new_amount);
                    } else {
                        bill["amount"] = json::from(new_amount);
                        map.payments.push(pay.clone());
                        used_payments.insert(json_int(&pay["id"])?);
                    }
                }
            }
        }
    }

    Ok(maps)
}

/// Returns true if the most recent payment toward a transaction
/// occurred within now minus the specified interval.
pub fn xact_has_payment_within(
    editor: &mut Editor,
    xact_id: i64,
    interval: &str,
) -> Result<bool, String> {
    let query = json::object! {
        "xact": xact_id,
        "payment_type": {"!=": "account_adjustment"}
    };

    let ops = json::object! {
        "limit": 1,
        "order_by": {"mp": "payment_ts DESC"}
    };

    let last_payment = editor.search_with_ops("mp", query, ops)?;

    if last_payment.len() == 0 {
        return Ok(false);
    }

    let payment = &last_payment[0];
    let intvl_secs = date::interval_to_seconds(interval)?;

    // Every payment has a payment_ts value
    let payment_ts = &payment["payment_ts"].as_str().unwrap();
    let payment_dt = date::parse_datetime(payment_ts)?;

    // Payments made before this time don't count.
    let window_start = Local::now() - Duration::seconds(intvl_secs);

    Ok(payment_dt > window_start)
}

pub fn generate_fines_for_resv(editor: &mut Editor, resv_id: i64) -> Result<(), String> {
    todo!()
}

pub fn generate_fines_for_circ(editor: &mut Editor, circ_id: i64) -> Result<(), String> {
    let circ = editor
        .retrieve("circ", circ_id)?
        .ok_or(format!("No such circulation: {circ_id}"))?;

    generate_fines_for_xact(
        editor,
        circ_id,
        circ["due_date"].as_str().unwrap(),
        json_int(&circ["target_copy"])?,
        json_int(&circ["circ_lib"])?,
        json_float(&circ["recurring_fine"])?,
        circ["fine_interval"].as_str().unwrap(),
        json_float(&circ["max_fine"])?,
        circ["grace_period"].as_str(),
    )
}

pub fn generate_fines_for_xact(
    editor: &mut Editor,
    xact_id: i64,
    due_date: &str,
    target_copy: i64,
    circ_lib: i64,
    mut recurring_fine: f64,
    fine_interval: &str,
    mut max_fine: f64,
    grace_period: Option<&str>,
) -> Result<(), String> {
    let mut settings = Settings::new(&editor);

    let fine_interval = date::interval_to_seconds(fine_interval)?;
    let mut grace_period = date::interval_to_seconds(grace_period.unwrap_or("0s"))?;
    let now = Local::now();

    if fine_interval == 0 || recurring_fine * 100.0 == 0.0 || max_fine * 100.0 == 0.0 {
        log::info!(
            "Fine generator skipping transaction {xact_id}
            due to 0 fine interval, 0 fine rate, or 0 max fine."
        );
        return Ok(());
    }

    // TODO add the bit about reservation time zone offsets

    let query = json::object! {
        "xact": xact_id,
        "btype": C::BTYPE_OVERDUE_MATERIALS,
    };

    let ops = json::object! {
        "flesh": 1,
        "flesh_fields": {"mb": ["adjustments"]},
        "order_by": {"mb": "billing_ts DESC"},
    };

    let fines = editor.search_with_ops("mb", query, ops)?;
    let mut current_fine_total = 0.0;
    for fine in fines.iter() {
        if !json_bool(&fine["voided"]) {
            current_fine_total += json_float(&fine["amount"])? * 100.0;
        }
        for adj in fine["adjustments"].members() {
            if !json_bool(&adj["voided"]) {
                current_fine_total -= json_float(&adj["amount"])? * 100.0;
            }
        }
    }

    log::info!(
        "Fine total for transaction {xact_id} is {:.2}",
        current_fine_total / 100.0
    );

    // Determine the billing period of the next fine to generate
    // based on the billing time of the most recent fine *which
    // occurred after the current due date*.  Otherwise, when a
    // due date changes, the fine generator will back-fill billings
    // for a period of time where the item was not technically overdue.
    let fines: Vec<JsonValue> = fines
        .iter()
        .filter(|f| f["billing_ts"].as_str().unwrap() > due_date)
        .map(|f| f.to_owned())
        .collect();

    let due_date_dt = date::parse_datetime(due_date)?;

    // First fine in the list (if we have one) will be the most recent.
    let last_fine_dt = match fines.get(0) {
        Some(f) => date::parse_datetime(&f["billing_ts"].as_str().unwrap())?,
        None => {
            grace_period = extend_grace_period(
                editor,
                circ_lib,
                grace_period,
                due_date_dt.clone(),
                None, // TODO?
                Some(&mut settings),
            )?;

            // If we have no fines, due date is the last fine time.
            due_date_dt
       }
    };

    if last_fine_dt > now {
        log::warn!("Transaction {xact_id} has futuer last fine date?");
        return Ok(());
    }

    if last_fine_dt == due_date_dt &&
        grace_period > 0 &&
        now.timestamp() < due_date_dt.timestamp() - grace_period {
        // We have no fines yet and we have a grace period and we
        // are still within the grace period.  New fines not yet needed.

        log::info!("Stil within grace period for circ {xact_id}");
        return Ok(());
    }

    // Generate fines for each past interval, including the one we are inside.
    let range = now.timestamp() - last_fine_dt.timestamp();
    let mut pending_fine_count = (range as f64 / fine_interval as f64).ceil() as i64;

    if pending_fine_count == 0 {
        // No fines to generate.
        return Ok(());
    }

    recurring_fine *= 100.0;
    max_fine *= 100.0;

    let skip_closed_check = json_bool(
        settings.get_value_at_org("circ.fines.charge_when_closed", circ_lib)?);

    let truncate_to_max_fine = json_bool(
        settings.get_value_at_org("circ.fines.truncate_to_max_fine", circ_lib)?);

    let timezone = match settings.get_value_at_org("lib.timezone", circ_lib)?.as_str() {
        Some(tz) => tz,
        None => "local",
    };


    Ok(())
}

pub fn extend_grace_period(
    editor: &mut Editor,
    context_org: i64,
    mut grace_period: i64,
    mut due_date: DateTime<Local>,
    org_hours: Option<&JsonValue>,
    settings: Option<&mut Settings>,
) -> Result<i64, String> {

    if grace_period < DAY_OF_SECONDS {
        // Only extended for >1day intervals.
        return Ok(grace_period);
    }

    let mut local_settings;
    let settings = match settings {
        Some(s) => s,
        None => {
            local_settings = Some(Settings::new(&editor));
            local_settings.as_mut().unwrap()
        }
    };

    let extend = json_bool(settings.get_value_at_org("circ.grace.extend", context_org)?);

    if !extend {
        // No extension configured.
        return Ok(grace_period);
    }

    let fetched_hours;
    let org_hours = match org_hours {
        Some(o) => o,
        None => {
            fetched_hours = editor.retrieve("aouhoo", context_org)?;
            match fetched_hours.as_ref() {
                Some(o) => o,
                // Hours of operation are required for extension
                None => return Ok(grace_period),
            }
        }
    };

    let mut close_days: HashSet<usize> = HashSet::new();
    let mut close_count = 0;
    for day in 0..6 {
        // day open/close are required fields.
        let open = org_hours[&format!("day_{day}_open")].as_str().unwrap();
        let close = org_hours[&format!("day_{day}_close")].as_str().unwrap();

        if open == "00:00:00" && close == "00:00:00" {
            close_count += 1;
            close_days.insert(day);
        }
    }

    if close_count == 7 {
        // Cannot extend if the branch is never open.
        return Ok(grace_period);
    }

    // Capture the original due date in epoch form.
    let orig_due_epoch = due_date.timestamp();

    let extend_into_closed = json_bool(
        settings.get_value_at_org("circ.grace.extend.into_closed", context_org)?);

    if extend_into_closed {
        // Merge closed dates trailing the grace period into the grace period.
        // Note to self: why add exactly one day?
        due_date = due_date + Duration::seconds(DAY_OF_SECONDS);
    }

    let extend_all = json_bool(
        settings.get_value_at_org("circ.grace.extend.all", context_org)?);

    if extend_all {
        // Start checking the day after the item was due.
        due_date = due_date + Duration::seconds(DAY_OF_SECONDS);
    } else {
        // Jump to the end of the grace period.
        due_date = due_date + Duration::seconds(grace_period);
    }

    let mut new_grace_period = grace_period;
    let mut counter = 0;
    let mut closed;

    // Scan at most 1 year of org close/hours info.
    while counter < 366 {
        counter += 1;
        closed = false;

        // Zero-based day of week.
        let weekday = due_date.naive_local().weekday().num_days_from_sunday();

        if close_days.contains(&(weekday as usize)) {
            closed = true;
            new_grace_period += DAY_OF_SECONDS;
            due_date = due_date + Duration::seconds(DAY_OF_SECONDS);

        } else {
            // Hours of operation say we're open, but we may have a
            // configured closed date.

            let timestamp = date::to_iso8601(&due_date);
            let query = json::object! {
                "org_unit": context_org,
                "close_start": {"<=": json::from(timestamp.clone())},
                "close_end": {">=": json::from(timestamp)},
            };

            let closed_days = editor.search("aoucd", query)?;

            if closed_days.len() > 0 {
                // Extend the due date out past this period of closed days.
                closed = true;
                for closed_day in closed_days.iter() {
                    let closed_dt = date::parse_datetime(
                        &closed_day["close_end"].as_str().unwrap() // required
                    )?;

                    if due_date <= closed_dt {
                        new_grace_period += DAY_OF_SECONDS;
                        due_date = due_date + Duration::seconds(DAY_OF_SECONDS);
                    }
                }
            } else {
                due_date = due_date + Duration::seconds(DAY_OF_SECONDS);
            }
        }

        if !closed && due_date.timestamp() > orig_due_epoch + new_grace_period {
            break;
        }
    }

    if new_grace_period > grace_period {
        grace_period = new_grace_period;
        log::info!("Grace period extended for circulation to {}", date::to_iso8601(&due_date));
    }

    Ok(grace_period)
}
