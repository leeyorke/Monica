// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright 2026 Bogdan Shapovalov and the Fury authors

//! A local MMDB as the proxy exit checker (issue #4).
//!
//! The checker field takes a URL or a path to a local MMDB file. A path
//! means: ask the default checker for the exit IP only — the address is
//! something only the far end can report — and resolve country, city,
//! timezone and coordinates from the file. Per field the file wins when it
//! has an answer and the checker is the fallback; `org` always comes from
//! the checker.

/// The exit IP goes out through the proxy either way; only the geo source
/// changes, so a path still asks here and not nowhere.
pub const DEFAULT_ENDPOINT: &str = "https://ipinfo.io/json";

/// Split the resolved checker value into the URL to ask and the local file
/// to resolve geo from, if it is one. Anything without a scheme is a path:
/// a pasted `ipinfo.io/json` was the 01.10.2026 loop — reqwest fails it at
/// send time as a bare "builder error" blamed on the proxy — so it reads as
/// a path that does not open rather than as a broken proxy.
pub fn split_checker(resolved: &str) -> (String, Option<String>) {
    let trimmed = resolved.trim().to_string();
    if trimmed.contains("://") {
        (trimmed, None)
    } else {
        (DEFAULT_ENDPOINT.to_string(), Some(trimmed))
    }
}

/// Country / city / timezone / "lat,lng" for one exit IP from a local file.
#[derive(Debug, Default)]
pub struct Geo {
    pub country: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
    pub location: Option<String>,
}

/// Rewrite an ipwho.is answer into the ipinfo shape every consumer reads.
///
/// The checker field takes any URL, but the field names the three readers
/// (`proxies.check`, `resolve_exit`, `diagnose::fetch_exit`) use are ipinfo's:
/// `country` for the ISO code, `timezone` a plain string, `loc` a "lat,lng".
/// ipwho.is spells all three differently -- `country_code` for the code (its
/// `country` is the full name, which would read "United States" where a flag
/// is expected), `timezone` a nested object, `latitude`/`longitude` separate
/// -- so without this it would parse to a blank country and no timezone.
///
/// Detected by the response's shape rather than the URL: `success` as a bool
/// plus a `country_code`, which ipinfo never sends, so a mirror or a
/// self-hosted ipwho.is on another host is handled the same. Anything that is
/// not that shape is returned untouched and keeps meaning ipinfo.
///
/// Only the fields the consumers actually read are mapped. ipwho.is carries
/// more (region, connection, flag); they stay in the body but nothing reads
/// them, same as ipinfo's `region` today.
pub fn normalise_checker_body(mut body: serde_json::Value) -> serde_json::Value {
    let is_ipwho = body.get("success").is_some_and(|v| v.is_boolean())
        && body.get("country_code").is_some_and(|v| v.is_string());
    if !is_ipwho {
        return body;
    }
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    // The ISO code is the whole point of the country in this UI (it keys the
    // flag), so `country_code` replaces the full-name `country` outright.
    if let Some(code) = obj.get("country_code").and_then(|v| v.as_str()) {
        obj.insert("country".into(), serde_json::json!(code));
    }
    // ipwho.is nests the zone: { "timezone": { "id": "Europe/Berlin", ... } }.
    // A flat string (an ipinfo-shaped answer that happens to carry a
    // `country_code`) is left as it stands.
    if obj.get("timezone").is_some_and(|v| v.is_object()) {
        let id = obj
            .get("timezone")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        obj.insert("timezone".into(), serde_json::json!(id));
    }
    // ipinfo folds the coordinates into one "lat,lng" string under `loc`;
    // ipwho.is keeps them apart. Compose `loc` so `parse_location` accepts it.
    if !obj.contains_key("loc") {
        // ipwho.is sends coordinates as numbers; a mirror may send strings.
        let num = |k: &str| {
            obj.get(k)
                .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        };
        if let (Some(lat), Some(lng)) = (num("latitude"), num("longitude")) {
            obj.insert("loc".into(), serde_json::json!(format!("{lat},{lng}")));
        }
    }
    body
}

/// Refused with the path in the message rather than failing later: a path
/// the operator pasted that does not open must read as one, not as silence.
pub fn lookup(db: &str, ip: &str) -> anyhow::Result<Geo> {
    let path = std::path::Path::new(db);
    let reader = maxminddb::Reader::open_readfile(path)
        .map_err(|e| anyhow::anyhow!("{db:?} does not open as an MMDB database: {e}"))?;
    let addr: std::net::IpAddr = ip
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{ip:?} is not an IP address"))?;
    let result = reader.lookup(addr).map_err(|e| anyhow::anyhow!("{e}"))?;
    // City carries country + city + timezone + coordinates; a Country file
    // decodes as one with only the country set, which is still an answer.
    if let Ok(Some(city)) = result.decode::<maxminddb::geoip2::City>() {
        let out = Geo {
            country: city.country.iso_code.map(str::to_string),
            city: city.city.names.english.map(str::to_string),
            timezone: city.location.time_zone.map(str::to_string),
            location: match (city.location.latitude, city.location.longitude) {
                (Some(lat), Some(lng)) => Some(format!("{lat},{lng}")),
                _ => None,
            },
        };
        if out.country.is_some() || out.city.is_some() || out.timezone.is_some() || out.location.is_some() {
            return Ok(out);
        }
    }
    if let Ok(Some(country)) = result.decode::<maxminddb::geoip2::Country>() {
        if let Some(iso) = country.country.iso_code {
            return Ok(Geo { country: Some(iso.to_string()), ..Default::default() });
        }
    }
    anyhow::bail!("{db:?} has no record for {ip}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_stays_a_url_and_a_path_becomes_the_default_plus_that_path() {
        assert_eq!(split_checker("https://ipinfo.io/json"), ("https://ipinfo.io/json".to_string(), None));
        assert_eq!(
            split_checker("/Users/me/city.mmdb"),
            (DEFAULT_ENDPOINT.to_string(), Some("/Users/me/city.mmdb".to_string()))
        );
        // The 01.10.2026 loop: no scheme is a path, never a proxy fault.
        let (endpoint, db) = split_checker("ipinfo.io/json");
        assert_eq!(endpoint, DEFAULT_ENDPOINT);
        assert!(db.is_some());
    }

    #[test]
    fn a_file_that_does_not_open_is_named() {
        let err = lookup("/definitely/not/there.mmdb", "8.8.8.8").unwrap_err();
        assert!(err.to_string().contains("not/there.mmdb"), "{err}");
    }

    #[test]
    fn an_ipwho_answer_is_rewritten_into_the_ipinfo_shape() {
        let ipwho = serde_json::json!({
            "ip": "134.195.101.171",
            "success": true,
            "country": "United States",
            "country_code": "US",
            "city": "Los Angeles",
            "timezone": { "id": "America/Los_Angeles", "abbr": "PST", "utc": "-08:00" },
            "latitude": 34.05,
            "longitude": -118.24,
        });
        let got = normalise_checker_body(ipwho);
        let s = |k: &str| got.get(k).and_then(|v| v.as_str());
        // The ISO code wins over the full name: the flag is keyed off it.
        assert_eq!(s("country"), Some("US"));
        assert_eq!(s("ip"), Some("134.195.101.171"));
        assert_eq!(s("city"), Some("Los Angeles"));
        // The nested zone flattens to the id the rest of the code expects.
        assert_eq!(s("timezone"), Some("America/Los_Angeles"));
        // Separate coordinates compose into ipinfo's "lat,lng".
        assert_eq!(s("loc"), Some("34.05,-118.24"));
    }

    #[test]
    fn an_ipinfo_answer_passes_through_untouched() {
        let ipinfo = serde_json::json!({
            "ip": "1.2.3.4",
            "country": "DE",
            "city": "Berlin",
            "timezone": "Europe/Berlin",
            "loc": "52.52,13.40",
            "org": "AS3320 Deutsche Telekom",
        });
        // No `success` bool, no `country_code`: not ipwho.is, so not touched.
        // `country` stays "DE" (an ipwho rewrite would have replaced it), and
        // the flat `timezone` string is left alone rather than blanked.
        assert_eq!(normalise_checker_body(ipinfo.clone()), ipinfo);
    }

    #[test]
    fn string_coordinates_from_a_mirror_still_compose_loc() {
        let mirrored = serde_json::json!({
            "ip": "1.2.3.4",
            "success": true,
            "country_code": "US",
            "timezone": { "id": "America/New_York" },
            "latitude": "40.7",
            "longitude": "-74.0",
        });
        let got = normalise_checker_body(mirrored);
        assert_eq!(
            got.get("loc").and_then(|v| v.as_str()),
            Some("40.7,-74")
        );
    }
}
