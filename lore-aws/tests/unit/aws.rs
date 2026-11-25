// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use time::Date;
use time::Month;
use time::UtcDateTime;

mod client;
mod creds;
mod differential;
mod dispatch;
mod dynamodb;
mod error;
mod http;
mod imds;
mod item;
mod json;
mod observe;
mod retry;
mod s3;
mod s3_dispatch;
mod serde;
mod sign;
mod sts;
pub(crate) mod test_server;
mod xml;

/// The UTC time `year`-`month`-`day` `hour`:`minute`:`second`.
pub(crate) fn utc(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> UtcDateTime {
    Date::from_calendar_date(year, Month::try_from(month).expect("a month"), day)
        .and_then(|date| date.with_hms(hour, minute, second))
        .expect("a valid date and time")
        .as_utc()
}
