// TODO(#22): Awareness of binding structure
// TODO(#23): Awareness of matched delimiters

use std::collections::{BinaryHeap, HashMap};
use std::fmt::Debug;
use std::io;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::{Condvar, Mutex, RwLock, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, debug_span, info};
use tree_sitter::{Language, Node, Tree};
use tree_sitter_edit::render;

use crate::check::Check;
use crate::edits::Edits;
use crate::id::NodeId;
use crate::node_types::NodeTypes;
use crate::original::Original;
use crate::stats::{self, Stats};
use crate::versioned::Versioned;

mod error;
mod task;

use error::ReductionError;
use task::{PrioritizedTask, Reduction, Task, TaskId};

use self::error::MultiPassReductionError;

#[inline]
fn node_size(node: &Node) -> usize {
    debug_assert!(node.start_byte() <= node.end_byte());
    node.end_byte() - node.start_byte()
}

#[derive(Debug)]
enum Interesting {
    Yes,
    No,
    Stale,
}

#[derive(Debug)]
struct Tasks {
    heap: RwLock<BinaryHeap<PrioritizedTask>>,
    task_id: AtomicUsize,
    push_signal: Condvar,
    push_signal_mutex: Mutex<bool>,
}

impl Tasks {
    fn new() -> Self {
        Tasks {
            heap: RwLock::new(BinaryHeap::new()),
            // TODO(lb): this is shared across runs...
            task_id: AtomicUsize::new(0),
            push_signal: Condvar::new(),
            push_signal_mutex: Mutex::new(false),
        }
    }

    fn push(&self, task: Task, priority: usize) -> Result<(), ReductionError> {
        {
            let mut w = self.heap.write()?;
            let id = self.task_id.fetch_add(1, atomic::Ordering::SeqCst);
            let ptask = PrioritizedTask {
                task,
                id: TaskId { id },
                priority,
            };
            debug!(
                event = "push",
                id = ptask.id.get(),
                kind = ptask.task.kind(),
                priority,
                heap_size = w.len(),
                "Pushing {} onto heap of size {}",
                ptask,
                w.len()
            );
            w.push(ptask);
        }
        // debug!("Heap size: {}", self.heap.read()?.len());
        self.push_signal.notify_one();
        Ok(())
    }

    fn push_all(&self, tasks: impl Iterator<Item = (Task, usize)>) -> Result<(), ReductionError> {
        {
            let mut w = self.heap.write()?;
            for (task, priority) in tasks {
                let id = self.task_id.fetch_add(1, atomic::Ordering::SeqCst);
                let ptask = PrioritizedTask {
                    task,
                    id: TaskId { id },
                    priority,
                };
                debug!(
                    event = "push",
                    id = ptask.id.get(),
                    kind = ptask.task.kind(),
                    priority,
                    "Pushing {} onto heap of size {}",
                    ptask,
                    w.len()
                );
                w.push(ptask);
            }
        }
        Ok(())
    }

    fn pop(&self) -> Result<Option<PrioritizedTask>, ReductionError> {
        // debug!("Heap size: {}", self.heap.read()?.len());
        let ptask = self.heap.write()?.pop();
        if let Some(pt) = &ptask {
            debug!(
                event = "pop",
                id = pt.id.get(),
                kind = pt.task.kind(),
                priority = pt.priority,
                "Popped {} from heap",
                pt,
            );
        }
        // debug!("Popped task with priority {}", task.as_ref().map(|t| t.priority).unwrap_or(0));
        Ok(ptask)
    }

    fn wait_for_push(&self, dur: Duration) -> Result<(), ReductionError> {
        match self.push_signal_mutex.try_lock() {
            Err(TryLockError::WouldBlock) => Ok(()),
            Err(TryLockError::Poisoned(p)) => Err(p.into()),
            Ok(lock) => {
                let _l = self.push_signal.wait_timeout(lock, dur)?;
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
struct Ctx<'a, T>
where
    T: Check + Send + Sync + 'static,
{
    delete_non_optional: bool,
    node_types: &'a NodeTypes,
    tasks: Tasks,
    edits: RwLock<Versioned<Edits>>,
    orig: Original,
    check: &'a T,
    min_task_size: usize,
    replacements: &'a HashMap<&'static str, &'static [&'static str]>,
}

struct ThreadCtx<'a, T>
where
    T: Check + Send + Sync + 'static,
{
    ctx: &'a Ctx<'a, T>,
    node_ids: HashMap<NodeId, Node<'a>>,
}

impl<'a, T> ThreadCtx<'a, T>
where
    T: Check + Send + Sync + 'static,
{
    fn new(ctx: &'a Ctx<T>) -> Self {
        let mut node_ids = HashMap::new();
        let mut queue = vec![ctx.orig.tree.root_node()];
        while let Some(node) = queue.pop() {
            node_ids.insert(NodeId::new(&node), node);
            queue.reserve(node.child_count());
            for child in node.children(&mut ctx.orig.tree.walk()) {
                queue.push(child);
            }
        }
        ThreadCtx { ctx, node_ids }
    }

    fn find(&self, id: &NodeId) -> Node<'a> {
        self.node_ids[id]
    }
}

impl<T> Ctx<'_, T>
where
    T: Check + Send + Sync + 'static,
{
    fn render(&self, edits: &Edits) -> io::Result<(bool, Vec<u8>)> {
        let mut text: Vec<u8> = Vec::with_capacity(self.orig.text.len() / 2);
        let changed = render(&mut text, &self.orig.tree, &self.orig.text, edits)?;
        Ok((changed, text))
    }

    fn _language(&self) -> Language {
        self.orig.tree.language()
    }

    fn _parse(&self, src: &[u8]) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        // TODO(lb): Incremental re-parsing
        parser
            .set_language(self._language())
            .expect("Error loading language");
        parser.parse(src, None).expect("Failed to parse")
    }

    /// Pop the highest-priority task from the task heap.
    fn pop_task(&self) -> Result<Option<PrioritizedTask>, ReductionError>
    where
        T: Sync,
    {
        // TODO(lb): What's the problem?
        // let point_o_one_seconds = Duration::new(0, 10000000);
        // Ok(self.tasks.wait_pop(point_o_one_seconds)?.map(|pt| pt.task))
        let task = self.tasks.pop()?;
        debug_assert!(
            task.as_ref().map(|t| t.priority).unwrap_or(usize::MAX) >= self.min_task_size
        );
        Ok(task)
    }

    fn push_task(&self, node: &Node, task: Task) -> Result<(), ReductionError> {
        self.push_prioritized_task(node_size(node), task)
    }

    fn push_prioritized_task(&self, priority: usize, task: Task) -> Result<(), ReductionError> {
        if priority < self.min_task_size {
            return Ok(());
        }
        // TODO(lb): Benchmark leaving this at 0
        self.tasks.push(task, priority)
    }

    fn push_explore_children(&self, node: Node) -> Result<(), ReductionError>
    where
        T: Check,
    {
        self.tasks.push_all(
            node.children(&mut self.orig.tree.walk())
                .filter(|child| node_size(child) > self.min_task_size)
                .map(|child| (Task::Explore(NodeId::new(&child)), node_size(&child))),
        )?;
        for _ in 0..node.child_count() {
            self.tasks.push_signal.notify_one();
        }
        Ok(())
    }

    fn add_task_edit(&self, task: &Task) -> Result<Option<Versioned<Edits>>, ReductionError> {
        let edits = self.edits.read()?;
        match task {
            Task::Explore(_) => {
                debug_assert!(false);
                Ok(Some(self.edits.read()?.clone()))
            }
            Task::Reduce(Reduction::Delete(node_id)) => {
                if edits.get().should_omit_id(node_id) {
                    return Ok(None);
                }
                Ok(Some(edits.mutate_clone(|e| e.omit_id(*node_id))))
            }
            Task::Reduce(Reduction::DeleteAll(node_ids)) => {
                if node_ids
                    .iter()
                    .all(|node_id| edits.get().should_omit_id(node_id))
                {
                    return Ok(None);
                }
                Ok(Some(edits.mutate_clone(|e| e.omit_ids(node_ids))))
            }
            Task::Reduce(Reduction::Replace { node_id, with }) => Ok(Some(
                edits.mutate_clone(|e| e.replace_id(*node_id, with.clone())),
            )),
        }
    }

    /// Check if the given edits yield an interesting tree. If so, and if the
    /// edits haven't been concurrently modified by another call to this
    /// function, replace the edits with the new ones.
    fn interesting(&self, ptask: &PrioritizedTask) -> Result<Interesting, ReductionError>
    where
        T: Check,
    {
        let id = ptask.id.get();
        let kind = ptask.task.kind();
        let priority = ptask.priority;
        // TODO(lb): Fields?
        let _span = debug_span!("Trying", id, kind, priority);
        '_outer: loop {
            let task = &ptask.task;
            let edits = if let Some(es) = self.add_task_edit(task)? {
                es
            } else {
                debug!(
                    event = "stale",
                    id = id,
                    kind = kind,
                    priority = priority,
                    "Task went stale: {}",
                    ptask
                );
                return Ok(Interesting::Stale);
            };
            // TODO(lb): Benchmark this:
            // if !self.edits.read()?.old_version(&edits) {
            //     return Ok(InterestingCheck::TryAgain);
            // }
            let (_changed, rendered) = self.render(edits.get())?;

            // For debugging:
            // let s = std::str::from_utf8(&rendered).unwrap();
            // eprintln!("{}", s);

            // TODO(lb): Don't introduce parse errors
            // let reparsed = self.parse(&rendered);
            // assert!({
            //     if reparsed.root_node().has_error() {
            //         self.orig.tree.root_node().has_error()
            //     } else {
            //         true
            //     }
            // });

            // Wait for the process to finish, exit early (try this reduction again)
            // if another thread beat us to it.

            let state = self.check.start(&rendered)?;

            // TODO(lb): Why is this slow?
            // while self.check.try_wait(&mut state)?.is_none() {
            //     // TODO(lb): Wait for 1/10 as long as the interestingness test takes
            //     // TODO(lb): Benchmark wait times
            //     // let point_o_o_one_seconds = Duration::new(0, 100000000);
            //     let point_o_one_seconds = Duration::new(0, 10000000);
            //     // let not_long = Duration::new(0, 1000);
            //     self.tasks.wait_for_push(point_o_one_seconds)?;
            //     match self.edits.try_read() {
            //         Err(_) => continue,
            //         Ok(l) => {
            //             if !l.old_version(&edits) {
            //                 self.check.cancel(state)?;
            //                 debug!("Canceled interestingness check");
            //                 continue 'outer;
            //             }
            //         }
            //     }
            // }

            let interesting: bool;
            {
                let _span = debug_span!("Waiting for command", id = id);
                interesting = self.check.wait(state)?;
            }

            if interesting {
                match self.edits.try_write() {
                    Err(_) => {
                        debug!(
                            event = "retry",
                            id = id,
                            kind = kind,
                            priority = priority,
                            "Retrying {}",
                            ptask
                        );
                        continue;
                    }
                    Ok(mut w) => {
                        let _span = debug_span!("Saving edits", id = id);
                        if !w.old_version(&edits) {
                            debug!(event = "retry", id, kind, priority, "Retrying {}", ptask);
                            continue;
                        }
                        *w = edits;
                        let size = rendered.len();
                        info!(id, kind, priority, size, "Reduced to size: {}", size);
                        debug!(
                            event = "interesting",
                            id,
                            kind,
                            priority,
                            "Interesting {}, new minimal program:\n{}",
                            kind,
                            std::str::from_utf8(&rendered).unwrap_or("<not UTF-8>")
                        );
                        return Ok(Interesting::Yes);
                    }
                }
            } else {
                debug!(
                    event = "uninteresting",
                    id, kind, priority, "Uninteresting {}", ptask
                );
                return Ok(Interesting::No);
            }
        }
    }
}

// TODO(#15): Refine with access to node-types.json
fn _is_list(_node: &Node) -> bool {
    false
}

fn explore<T: Check + Send + Sync>(
    tctx: &ThreadCtx<T>,
    node_id: NodeId,
) -> Result<(), ReductionError> {
    // TODO(lb): Include kind in explore task to avoid find
    let node = tctx.find(&node_id);
    let _span = debug_span!("Exploring", id = node_id.get());
    debug!("Exploring {}...", tctx.find(&node_id).kind());
    if let Some(replaces) = tctx.ctx.replacements.get(node.kind()) {
        // TODO(lb): Benchmark locking tasks and pushing all at once
        for replace in *replaces {
            tctx.ctx.push_task(
                &node,
                Task::Reduce(Reduction::Replace {
                    node_id,
                    with: String::from(*replace),
                }),
            )?;
        }
    }
    if tctx.ctx.node_types.optional_node(&node) || tctx.ctx.delete_non_optional {
        tctx.ctx
            .push_task(&node, Task::Reduce(Reduction::Delete(node_id)))?;
    } else {
        // If this node has some children/fields that can have multiple nodes,
        // try deleting all of them at once (by kind).
        let child_list_types = tctx.ctx.node_types.list_types(&node);
        if !child_list_types.is_empty() {
            // TODO(lb): Benchmark locking tasks and pushing all at once
            for node_kind in child_list_types {
                let mut batch = Vec::new();
                let mut batch_size = 0;
                for subkind in tctx.ctx.node_types.subtypes(&node_kind) {
                    for child in node.children(&mut tctx.ctx.orig.tree.walk()) {
                        if child.kind() == subkind {
                            batch.push(NodeId::new(&child));
                            batch_size += child.end_byte() - child.start_byte();
                        }
                    }
                }
                tctx.ctx
                    .push_prioritized_task(batch_size, Task::Reduce(Reduction::DeleteAll(batch)))?;
            }
        }
        tctx.ctx.push_explore_children(node)?;
    }
    Ok(())
}

fn dispatch<T: Check + Send + Sync>(
    tctx: &ThreadCtx<T>,
    ptask: PrioritizedTask,
) -> Result<(), ReductionError> {
    match ptask.task {
        Task::Explore(node_id) => explore(tctx, node_id),
        Task::Reduce(Reduction::Delete(node_id)) => {
            let _span = debug_span!("Reducing", id = node_id.get());
            match tctx.ctx.interesting(&ptask)? {
                Interesting::Yes => {
                    // This tree was deleted, no need to recurse on children
                    Ok(())
                }
                Interesting::No => {
                    tctx.ctx.push_explore_children(tctx.find(&node_id))?;
                    Ok(())
                }
                // This tree and all of its children were deleted by an edit in
                // a competing thread
                Interesting::Stale => Ok(()),
            }
        }
        Task::Reduce(Reduction::DeleteAll(_)) => {
            // No need to check whether it was interesting, because the children will be
            // individually handled by `delete`.
            let _ = tctx.ctx.interesting(&ptask);
            Ok(())
        }
        Task::Reduce(Reduction::Replace { node_id, .. }) => {
            let _span = debug_span!("Reducing", id = node_id.get());
            match tctx.ctx.interesting(&ptask)? {
                Interesting::Yes => {
                    // This tree was replaced, no need to recurse on children
                    Ok(())
                }
                Interesting::No => {
                    tctx.ctx.push_explore_children(tctx.find(&node_id))?;
                    Ok(())
                }
                Interesting::Stale => Ok(()),
            }
        }
    }
}

/// Main function for each thread
fn work<T: Check + Send + Sync>(ctx: &Ctx<T>, num_threads: usize) -> Result<(), ReductionError> {
    static IDLE_THREADS: AtomicUsize = AtomicUsize::new(0);
    // Since IDLE_THREADS is static, decrement for second and later passes
    let idle = IDLE_THREADS.load(atomic::Ordering::Acquire);
    if idle > 0 {
        IDLE_THREADS.store(idle - 1, atomic::Ordering::Release)
    }

    let tctx = ThreadCtx::new(ctx);
    let mut idle = false;
    // Quit if all threads are idle and there are no remaining tasks
    while IDLE_THREADS.load(atomic::Ordering::Acquire) < num_threads {
        if idle {
            // TODO(lb): Integrate waiting into pop?
            // TODO(lb): Benchmark the duration
            // let point_o_one_seconds = Duration::new(0, 10000000);
            let not_long = Duration::new(0, 100000);
            tctx.ctx.tasks.wait_for_push(not_long)?;
            IDLE_THREADS.fetch_sub(1, atomic::Ordering::Release);
        }
        while let Some(ptask) = tctx.ctx.pop_task()? {
            debug!(
                id = ptask.id.get(),
                kind = ptask.task.kind(),
                priority = ptask.priority,
                "Popped {}",
                ptask
            );
            dispatch(&tctx, ptask)?;
        }
        let num_idle = IDLE_THREADS.fetch_add(1, atomic::Ordering::Release);
        debug!(
            idle = num_idle + 1,
            threads = num_threads,
            "Idling {} / {}...",
            num_idle + 1,
            num_threads
        );
        idle = true;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Config<T> {
    pub check: T,
    pub delete_non_optional: bool,
    pub jobs: usize,
    // TODO(lb): Maybe per-pass, benchmark
    pub min_reduction: usize,
    pub replacements: HashMap<&'static str, &'static [&'static str]>,
}

pub fn treereduce<T: Check + Debug + Send + Sync + 'static>(
    node_types: &NodeTypes,
    orig: Original,
    conf: &Config<T>,
) -> Result<(Original, Edits), ReductionError> {
    if orig.text.is_empty() {
        return Ok((orig, Edits::new()));
    }

    let _span = debug_span!("Pass");
    info!("Original size: {}", orig.text.len());
    // eprintln!("{}", orig.tree.root_node().to_sexp());
    // TODO(#25): SIGHUP handler to save intermediate progress
    let jobs = std::cmp::max(1, conf.jobs);
    let min_reduction = std::cmp::max(1, conf.min_reduction);
    let tasks = Tasks::new();
    let root = orig.tree.root_node();
    let root_id = NodeId::new(&root);
    tasks.push(Task::Explore(root_id), node_size(&root))?;
    let ctx = Ctx {
        delete_non_optional: conf.delete_non_optional,
        node_types,
        tasks,
        edits: RwLock::new(Versioned::new(Edits::new())),
        orig,
        check: &conf.check,
        min_task_size: min_reduction,
        replacements: &conf.replacements,
    };

    thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| work(&ctx, jobs));
        }
    });

    debug_assert!(ctx.tasks.heap.read()?.is_empty());
    let edits = ctx.edits.read()?.clone();
    Ok((ctx.orig, edits.extract()))
}

// Don't care about parse errors, we're maintaining the interestingness
fn parse(language: tree_sitter::Language, code: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .expect("Failed to set tree-sitter parser language");
    parser.parse(code, None).expect("Failed to parse code")
}

pub fn treereduce_multi_pass<T: Clone + Check + Debug + Send + Sync + 'static>(
    language: tree_sitter::Language,
    node_types: &NodeTypes,
    mut orig: Original,
    conf: &Config<T>,
    max_passes: Option<usize>,
) -> Result<(Original, Stats), MultiPassReductionError> {
    let mut stats = Stats::new();
    stats.start_size = orig.text.len();
    let reduce_start = Instant::now();
    let mut passes_done = 0;
    while passes_done < max_passes.unwrap_or(usize::MAX) {
        let pass_start_size = orig.text.len();
        info!(
            "Starting pass {} / {}",
            passes_done + 1,
            max_passes
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string())
        );
        let pass_start = Instant::now();

        let (new, edits) = treereduce(node_types, orig, conf)?;
        orig = new;
        let mut new_src = Vec::new();
        tree_sitter_edit::render(&mut new_src, &orig.tree, orig.text.as_slice(), &edits)?;
        let text = std::str::from_utf8(&new_src)?.to_string();
        orig = Original::new(parse(language, &text), new_src);

        passes_done += 1;
        let pass_stats = stats::Pass {
            duration: pass_start.elapsed(),
            start_size: pass_start_size,
            end_size: orig.text.len(),
        };
        debug!(
            "Pass {} duration: {}ms",
            passes_done,
            pass_stats.duration.as_millis()
        );
        stats.passes.push(pass_stats);

        if edits.is_empty() {
            info!("Qutting after pass {} found no reductions", passes_done);
            break;
        }
    }
    stats.duration = reduce_start.elapsed();
    info!("Total time: {}ms", stats.duration.as_millis());
    stats.end_size = orig.text.len();
    Ok((orig, stats))
}
