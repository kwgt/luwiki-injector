/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */
//!
//! MS1.2の技術実証を統合するテストターゲット
//!

#[path = "ms1_2/git_probe.rs"]
mod git_probe;
#[path = "ms1_2/limits_probe.rs"]
mod limits_probe;
#[path = "ms1_2/runtime_probe.rs"]
mod runtime_probe;
#[path = "ms1_2/support.rs"]
mod support;
#[path = "ms1_2/zip_probe.rs"]
mod zip_probe;
