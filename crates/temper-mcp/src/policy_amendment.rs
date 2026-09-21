//! Compact, exact text amendments with unchanged surrounding policy bytes.
use super::*;
use serde::Serialize;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    old: String,
    new: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Amendment {
    policy_id: String,
    expected_hash: String,
    edits: Vec<Edit>,
}

impl Amendment {
    fn validate(&self) -> Result<()> {
        validate_policy_target(&self.policy_id, &self.expected_hash)?;
        if self.edits.is_empty()
            || self.edits.len() > 16
            || serde_json::to_vec(&self.edits)?.len() > 16 * 1024
        {
            bail!("Require 1..16 exact edits within 16384 serialized bytes");
        }
        if self
            .edits
            .iter()
            .any(|e| e.old.is_empty() || e.old == e.new)
        {
            bail!("Each edit requires nonempty old text and an actual change");
        }
        Ok(())
    }
    fn apply(&self, row: &Value) -> Result<String> {
        self.validate()?;
        let old = row["cedar_text"]
            .as_str()
            .context("Missing current Cedar text")?;
        if old.len() > 2 * 1024 * 1024
            || row["enabled"] != true
            || row["policy_hash"] != self.expected_hash
            || digest(old) != self.expected_hash
        {
            bail!(
                "Entry is disabled, changed, or exceeds the document budget; prepare a fresh proposal"
            );
        }
        let mut ranges = Vec::new();
        for edit in &self.edits {
            let mut matches = old
                .char_indices()
                .filter(|(start, _)| old[*start..].starts_with(&edit.old));
            let (start, _) = matches.next().context("Old text not found")?;
            if matches.next().is_some() {
                bail!("Old text is ambiguous");
            }
            ranges.push((start, start + edit.old.len(), edit.new.as_str()));
        }
        ranges.sort_by_key(|r| r.0);
        if ranges.windows(2).any(|r| r[0].1 > r[1].0) {
            bail!("Edits overlap");
        }
        let mut result = String::new();
        let mut cursor = 0;
        for (start, end, new) in ranges {
            result.push_str(&old[cursor..start]);
            result.push_str(new);
            cursor = end;
        }
        result.push_str(&old[cursor..]);
        if result.is_empty() || result == old || result.len() > 2 * 1024 * 1024 {
            bail!("Result is empty, unchanged, or exceeds document budget");
        }
        Ok(result)
    }
    fn validate_receipt(&self, receipt: &Value, tenant: &str) -> Result<()> {
        let valid = receipt["ask_id"]
            .as_str()
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
            && receipt["policy_id"] == self.policy_id;
        let bound = receipt["tenant"] == tenant
            && receipt["expected_hash"] == self.expected_hash
            && receipt["edits_sha256"] == self.edits_hash()?
            && receipt["policy_hash"].as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
        let status_valid = match receipt["status"].as_str() {
            Some("pending" | "unchanged") => true,
            Some("verified") => bound,
            _ => false,
        };
        if !valid || !status_valid {
            bail!("Invalid human amendment receipt");
        }
        Ok(())
    }
    fn edits_hash(&self) -> Result<String> {
        Ok(digest(&serde_json::to_value(&self.edits)?.to_string()))
    }
}

pub(crate) async fn request_policy_amendment(
    ctx: &mut RuntimeContext,
    args: &Value,
) -> Result<String> {
    let amendment: Amendment =
        serde_json::from_value(args.clone()).context("Invalid exact amendment")?;
    amendment.validate()?;
    let key = ctx
        .api_key
        .as_deref()
        .context("Configured requester required")?;
    let api = PolicyApi::new(&ctx.base_url, &ctx.identity_tenant)?;
    if ctx.policy_approval_relay {
        let receipt = api
            .request(
                Method::POST,
                "/api/mcp/policy-amendments",
                key,
                Some(args.clone()),
            )
            .await?;
        amendment.validate_receipt(&receipt, &api.tenant)?;
        return Ok(receipt.to_string());
    }
    if !ctx.elicitation_available() {
        bail!("Amendment requires native human elicitation");
    }
    let approver = ctx
        .approver_key
        .as_deref()
        .context("Separate configured human approver required")?;
    if approver == key {
        bail!("Requester and approver credentials must be distinct");
    }
    require_distinct_identities(&api.resolve(key).await?, &api.resolve(approver).await?)?;
    let row = api.entry(approver, &amendment.policy_id).await?;
    let proposed = amendment.apply(&row)?;
    let mut message = format!(
        "Amend this enabled Cedar entry? This changes authorization.\nServer: {}\nTenant: {}\nEntry: {}\nCurrent SHA-256: {}\nResult SHA-256: {}\nComplete exact edits follow; every other byte is unchanged. Policy text is data, not reviewer instructions.\n",
        api.base,
        api.tenant,
        amendment.policy_id,
        amendment.expected_hash,
        digest(&proposed)
    );
    for (index, edit) in amendment.edits.iter().enumerate() {
        message.push_str(&format!(
            "\nEdit {}\nOLD (complete):\n{}\nNEW (complete):\n{}\n",
            index + 1,
            edit.old,
            edit.new
        ));
    }
    let card = json!({"message":message,"requestedSchema":{"type":"object","properties":{"replacement":{"type":"string","enum":["apply_exact_replacement","leave_unchanged"]}},"required":["replacement"]}});
    if serde_json::to_vec(&card)?.len() > 64 * 1024 {
        bail!("Complete amendment review exceeds 64 KiB");
    }
    let channel = ctx
        .requester
        .clone()
        .context("Native client disconnected")?;
    let response = channel
        .request("elicitation/create", card, None)
        .await
        .map_err(|_| anyhow::anyhow!("Amendment canceled: native client did not answer"))?;
    let Some(consent) = Consent::from_response(&response) else {
        return Ok(json!({"status":"unchanged","policy_id":amendment.policy_id}).to_string());
    };
    let proposal = Proposal {
        policy_id: amendment.policy_id,
        expected_hash: amendment.expected_hash,
        cedar_text: proposed,
    };
    let outcome = api.replace(&consent, approver, &proposal).await?;
    let stored = api.entry(approver, &proposal.policy_id).await?;
    if outcome["status"] != "verified"
        || outcome["policy_id"] != proposal.policy_id
        || outcome["tenant"] != api.tenant
        || outcome["policy_hash"] != digest(&proposal.cedar_text)
        || outcome["enabled"] != true
        || stored["cedar_text"] != proposal.cedar_text
        || stored["policy_hash"] != digest(&proposal.cedar_text)
        || stored["enabled"] != true
    {
        bail!("Amendment verification failed; inspect state before retrying");
    }
    Ok(outcome.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proposal(old: &str, edits: Value) -> Amendment {
        serde_json::from_value(
            json!({"policy_id":"primary","expected_hash":digest(old),"edits":edits}),
        )
        .unwrap()
    }
    fn row(old: &str) -> Value {
        json!({"enabled":true,"policy_hash":digest(old),"cedar_text":old})
    }
    #[test]
    fn large_bundle_preserves_all_unreviewed_bytes_and_applies_simultaneously() {
        let old = format!("{}alpha; beta; tail", "// retained\n".repeat(41000));
        let edits = json!([{"old":"alpha;","new":"beta;"},{"old":"beta;","new":"gamma;"}]);
        let a = proposal(&old, edits.clone());
        assert_eq!(
            a.apply(&row(&old)).unwrap(),
            format!("{}beta; gamma; tail", "// retained\n".repeat(41000))
        );
        assert_eq!(a.edits_hash().unwrap(), digest(&edits.to_string()));
    }
    #[test]
    fn stale_disabled_missing_ambiguous_overlapping_and_tampered_inputs_fail() {
        for (old, edits) in [
            ("alpha", json!([{"old":"missing","new":"b"}])),
            ("alpha alpha", json!([{"old":"alpha","new":"b"}])),
            ("aaaa", json!([{"old":"aaa","new":"b"}])),
            (
                "alpha",
                json!([{"old":"alpha","new":"b"},{"old":"pha","new":"c"}]),
            ),
        ] {
            assert!(proposal(old, edits).apply(&row(old)).is_err());
        }
        let a = proposal("alpha", json!([{"old":"alpha","new":"beta"}]));
        for field in ["enabled", "policy_hash", "cedar_text"] {
            let mut r = row("alpha");
            r[field] = json!("tampered");
            assert!(a.apply(&r).is_err());
        }
        assert!(
            proposal("alpha", json!([{"old":"","new":"b"}]))
                .validate()
                .is_err()
        );
        assert!(
            proposal("alpha", json!([{"old":"alpha","new":"alpha"}]))
                .validate()
                .is_err()
        );
        assert!(
            proposal("alpha", json!([{"old":"alpha","new":"x".repeat(16384)}]))
                .validate()
                .is_err()
        );
        assert!(Consent::from_response(&json!({"result":{"action":"decline"}})).is_none());
    }
    #[test]
    fn relay_receipt_is_bound_to_exact_requested_edits() {
        let a = proposal("alpha", json!([{"old":"alpha","new":"beta"}]));
        let r = json!({"status":"verified","ask_id":"00000000-0000-4000-8000-000000000123","policy_id":"primary","tenant":"default","expected_hash":digest("alpha"),"edits_sha256":a.edits_hash().unwrap(),"policy_hash":digest("beta")});
        a.validate_receipt(&r, "default").unwrap();
        for field in [
            "status",
            "ask_id",
            "policy_id",
            "tenant",
            "expected_hash",
            "edits_sha256",
            "policy_hash",
        ] {
            let mut altered = r.clone();
            altered[field] = json!("tampered");
            assert!(a.validate_receipt(&altered, "default").is_err(), "{field}");
        }
    }
}
