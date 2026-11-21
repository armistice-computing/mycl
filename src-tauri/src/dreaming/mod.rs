//! Dreaming: the cloud-1 dream cycle ported to gardend (Phase 2 of the
//! dreaming port, graph doc dreaming-port-plan-2026-10-02). The model call
//! goes through `crate::dreamer`; everything before and after it is here.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

pub(crate) mod articulate;
pub(crate) mod astrology;
pub(crate) mod calibration;
pub(crate) mod cycle;
#[cfg(all(test, feature = "headless"))]
mod cycle_fence_tests;
pub(crate) mod dream_wires;
pub(crate) mod exclusions;
pub(crate) mod graph_settings;
pub(crate) mod journal;
pub(crate) mod mechanics;
pub(crate) mod rng;
pub(crate) mod scanner;
pub(crate) mod scheduler;
pub(crate) mod temporal;
pub(crate) mod weave;
pub(crate) mod wire_graph;
