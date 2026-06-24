//! punch item 8 (b3-kill-spec) LIVE PINS — the descendant-tree kill leg.
//!
//! `sb stop`'s per-pid ladder (zmx pane + wrapper pid + claude pid) left
//! claude's GRANDCHILDREN running (P2 panel-unanimous). The shipped fix is the
//! descendant-tree leg: snapshot the claude pid's ppid-descendants while the
//! tree is intact, stamp each with `(pid, start-time)` (exec-proof identity),
//! and sweep bottom-up with a per-victim identity recheck before EVERY signal
//! (`effects::kill_pid_tree`). These rows drive the REAL machinery on REAL
//! processes (own children of this test — never a shared/live resource):
//!
//!   1. a backgrounded grandchild is DEAD after the sweep, while a NEIGHBOR
//!      process with the same cmdline (different parent) SURVIVES;
//!   2. a victim whose snapshot identity no longer matches (the reused-pid
//!      shape, simulated with a stale start stamp) is NEVER signaled.
//!
//! The self-stop guard + bottom-up ordering are pinned by the pure units in
//! `dispatch::kill::descendant_tests`.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dispatch::effects::{
    is_pid_alive, kill_pid, kill_pid_tree, parse_ps_rows, proc_start_ms, ProcRow,
};
use dispatch::kill::{descendant_kill_list, sweep_root_allowed};

/// Full-table ps rows (the same read the verb's snapshot uses).
fn ps_rows() -> HashMap<i32, ProcRow> {
    let out = Command::new("ps")
        .args(["-eo", "pid=,ppid=,command="])
        .output()
        .expect("spawn ps");
    parse_ps_rows(&String::from_utf8_lossy(&out.stdout))
}

/// Spawn `bash -c <script>` as our own child (Stdio nulled so nothing leaks
/// into the test harness output).
fn spawn_bash(script: &str) -> Child {
    Command::new("bash")
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bash")
}

/// Poll until `cond` or the deadline; returns whether it fired.
fn wait_for(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[test]
fn grandchild_dead_after_sweep_neighbor_survives() {
    // The session-shaped tree: child bash backgrounds a sleep (the grandchild)
    // then waits — exactly the survivor class the per-pid ladder missed.
    let mut child = spawn_bash("sleep 300 & wait");
    let child_pid = child.id() as i32;
    // The NEIGHBOR: same cmdline shape (`sleep 300`), different parent (this
    // test process directly) — the wrong-victim control.
    let mut neighbor = Command::new("/bin/sleep")
        .arg("300")
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn neighbor");
    let neighbor_pid = neighbor.id() as i32;

    // Wait for the grandchild to appear under the child bash.
    let appeared = wait_for(Duration::from_secs(5), || {
        let rows = ps_rows();
        rows.iter().any(|(_, r)| r.ppid == child_pid)
    });
    assert!(appeared, "grandchild never appeared under the child bash");

    // The snapshot, as the verb takes it: descendants of the (here: trivially
    // verified — it is our own child) root, stamped with current start times.
    let rows = ps_rows();
    let my_pid = std::process::id() as i32;
    let list = descendant_kill_list(child_pid, my_pid, &rows);
    assert!(
        !list.is_empty(),
        "descendant list must contain the grandchild: {list:?}"
    );
    assert!(
        !list.contains(&neighbor_pid),
        "the neighbor (different parent) must never be enumerated"
    );
    let victims: Vec<(i32, i64)> = list
        .iter()
        .filter_map(|&p| proc_start_ms(p).map(|s| (p, s)))
        .collect();
    assert!(!victims.is_empty(), "victims must carry start stamps");
    let grandchild_pid = victims[0].0;

    // The sweep (short grace — sleeps die on SIGTERM immediately).
    let leftovers = kill_pid_tree(&victims, 1_000);
    assert!(leftovers.is_empty(), "sweep leftovers: {leftovers:?}");

    // PIN 1: the grandchild is dead (its parent bash reaps it on `wait`).
    let grandchild_gone = wait_for(Duration::from_secs(3), || !is_pid_alive(grandchild_pid));
    assert!(
        grandchild_gone,
        "grandchild {grandchild_pid} must be dead after the sweep"
    );
    // PIN 2: the same-cmdline NEIGHBOR survives — per-victim identity means
    // no name/pattern class can widen the blast radius.
    assert!(
        is_pid_alive(neighbor_pid),
        "neighbor {neighbor_pid} must survive the sweep"
    );

    // Cleanup (own children only).
    kill_pid(child_pid, 500);
    kill_pid(neighbor_pid, 500);
    let _ = child.wait();
    let _ = neighbor.wait();
}

#[test]
fn stale_identity_victim_is_never_signaled() {
    // A live process whose snapshot stamp is WRONG (60s off — far outside
    // TREE_KILL_START_SLACK_MS) models the reused-pid shape: the pid in the
    // snapshot no longer is the process holding it. It must never be signaled.
    let mut victim = Command::new("/bin/sleep")
        .arg("300")
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn victim");
    let pid = victim.id() as i32;
    // Production-shape identity stamp: the registry records `started_at ≈
    // wall-clock now` at registration, and the kill gate compares it to the live
    // `proc_start_ms` within TREE_KILL_START_SLACK_MS. Recording
    // `proc_start_ms(pid).expect(...)` here instead PANICS under spawn-storm load
    // — a sub-second victim's `ps -o etime=` misparse hits the WP-E range guard →
    // `None`. Capture a wall-clock now-ms right after spawn, the way production
    // does, for BOTH the stale offset and the control.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let leftovers = kill_pid_tree(&[(pid, now_ms - 60_000)], 300);
    // Not signaled: still alive, and NOT reported as a leftover (a pid that
    // fails the identity gate is not ours to report).
    assert!(
        is_pid_alive(pid),
        "identity-mismatched pid must never be signaled"
    );
    assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");

    // Control: with the MATCHING (production-shape wall-clock) stamp the same
    // process dies. By kill time the `sleep 300` victim is >1s old, so its live
    // `proc_start_ms` parses cleanly; `now_ms` (captured at spawn+ε) differs from
    // the victim's real birth by only a few ms ≪ slack ⇒ identity matches ⇒ SIGTERM.
    let leftovers = kill_pid_tree(&[(pid, now_ms)], 1_000);
    assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    let gone = wait_for(Duration::from_secs(3), || {
        // A SIGTERM'd direct child stays a ZOMBIE until waited — reap it and
        // judge by wait, not is_pid_alive (kill(pid,0) is true for zombies).
        victim.try_wait().map(|s| s.is_some()).unwrap_or(true)
    });
    assert!(gone, "true-identity victim must die");
    let _ = victim.wait();
}

/// b3 adversarial concern 1 PIN (the sweep-root gate, on a REAL tree): when
/// the root carries NO positive witness (cmdline unreadable AND no start-time
/// confirmation), the verb withholds the SUBTREE sweep even though the lone
/// root-kill would still proceed. Reproduces the exact verb composition: a
/// live root with a real descendant subtree, gated by `sweep_root_allowed`
/// with the no-witness inputs injected explicitly — the gate is false, so the
/// snapshot the verb would feed kill_pid_tree is EMPTY (zero tree kills), and
/// the stranger's grandchildren are left alone (the accepted leak direction).
#[test]
fn no_witness_root_withholds_subtree_sweep() {
    // A live tree: bash with a backgrounded grandchild — stands in for the
    // STRANGER that reused a stale registry pid.
    let mut stranger = spawn_bash("sleep 300 & wait");
    let root_pid = stranger.id() as i32;
    let appeared = wait_for(Duration::from_secs(5), || {
        ps_rows().iter().any(|(_, r)| r.ppid == root_pid)
    });
    assert!(appeared, "stranger grandchild never appeared");

    // The verb's gate, with the no-witness condition injected EXPLICITLY:
    // cmdline read failed (None) AND no registry started_at to confirm
    // against. This is the cell pid_is_foreign treats as not-foreign (so the
    // lone kill proceeds) but which carries no positive root evidence.
    let gate_ok = sweep_root_allowed(
        /*cmdline*/ None,
        "claude",
        proc_start_ms(root_pid),
        None,
    );
    assert!(
        !gate_ok,
        "no-witness root must NOT be allowed to root a subtree sweep"
    );

    // Therefore the verb forms an EMPTY victim set (the `if sweep_root_ok`
    // branch is skipped) — kill_pid_tree on empty is a no-op, zero kills.
    let victims: Vec<(i32, i64)> = if gate_ok {
        let rows = ps_rows();
        descendant_kill_list(root_pid, std::process::id() as i32, &rows)
            .into_iter()
            .filter_map(|p| proc_start_ms(p).map(|s| (p, s)))
            .collect()
    } else {
        Vec::new()
    };
    assert!(
        victims.is_empty(),
        "no-witness root must produce no sweep victims"
    );
    let leftovers = kill_pid_tree(&victims, 300);
    assert!(leftovers.is_empty());

    // The stranger's tree is untouched (we never swept it). Control: a
    // positive witness (the cmdline IS our session program) WOULD root.
    assert!(is_pid_alive(root_pid), "stranger root must be untouched");
    assert!(
        sweep_root_allowed(Some("/usr/bin/claude"), "claude", None, Some(1_000)),
        "a cmdline-matched root must root the sweep (control)"
    );

    // Cleanup our own tree.
    kill_pid(root_pid, 500);
    let _ = stranger.wait();
}
