//! Remote forwarding: one wire op to a client, stdout rendered
//! through the §5 extract-or-verbatim rule.

use anyhow::{Context as _, Result};

/// Forward one wire op to a client and print remote stdout.
/// The remote argv is `<qroot>/bin/shunt <tail...> --root <qroot>`;
/// transport is `ssh` or the `localhost` loopback (design.md §5).
pub(crate) fn remote(name: &str, tail: &[&str], stdin: Option<&str>, expect_json: bool) -> Result<()> {
    use crate::server::registry;
    use crate::server::transport::{queue_exec, transport_for};
    let base = registry::load_client(name)?;
    let queue = registry::queue_names(&base).into_iter().next().context("client has no registered queues")?;
    let resolved = registry::resolve_queue(&base, Some(&queue))?;
    let t = transport_for(&resolved.client);
    let out = queue_exec(t.as_ref(), &resolved, tail, stdin)?;
    print!("{}", remote_render(&out, expect_json)?);
    Ok(())
}

/// Queue controls must resolve an execution target, never the machine row.
pub(crate) fn remote_queue(name: &str, queue: Option<&str>, tail: &[&str], stdin: Option<&str>, expect_json: bool) -> Result<()> {
    use crate::server::transport::{queue_exec, transport_for};
    let resolved = crate::server::registry::load_queue(name, queue)?;
    let t = transport_for(&resolved.client);
    let out = queue_exec(t.as_ref(), &resolved, tail, stdin)?;
    print!("{}", remote_render(&out, expect_json)?);
    Ok(())
}

/// What `remote` prints for one chunk of remote stdout. Wire ops
/// answer a `{"protocol":1,…}` object that can sit inside login-shell
/// noise (motd/.bashrc banners — design.md §5): extract renders the
/// object pretty and drops the noise. A JSON object that fails
/// extraction — wrong version or no `protocol` at all — is binary
/// drift: surface §5's steering error rather than printing the
/// stale line raw. Pure-noise output still prints verbatim.
///
/// `expect_json` is the op's §5 response kind: wire ops answering a
/// `{protocol:1,…}` object pass `true`; text ops (`logs`) pass `false`
/// and their stdout prints verbatim — a log line that happens to be a
/// protocol-shaped JSON object is log content, never a wire answer.
pub(super) fn remote_render(out: &str, expect_json: bool) -> Result<String> {
    if !expect_json {
        return Ok(verbatim(out));
    }
    match crate::proto::wire::extract(out) {
        Ok(v) => Ok(format!("{}\n", serde_json::to_string_pretty(&v)?)),
        Err(e) => {
            let drift = out.lines().any(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok().is_some_and(|v| v.is_object()));
            if drift {
                return Err(e);
            }
            Ok(verbatim(out))
        }
    }
}

/// Pass remote stdout through unchanged, adding one trailing newline
/// when the stream lacked it (keeps the user's next prompt clean).
fn verbatim(out: &str) -> String {
    if out.ends_with('\n') {
        out.to_string()
    } else {
        format!("{out}\n")
    }
}
