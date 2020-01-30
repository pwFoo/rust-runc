/*
 * Copyright 2020 fsyncd, Berlin, Germany.
 * Additional material, copyright of the containerd authors.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(serialize = "lowercase", deserialize = "lowercase"))]
pub enum EventType {
    /// Statistics
    Stats,
    /// Out of memory
    OOM,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Event type
    #[serde(rename = "type")]
    pub event_type: Option<EventType>,
    /// Contained id
    pub id: Option<String>,
    /// Event data payload (only for statistics)
    #[serde(rename = "data")]
    pub stats: Option<Stats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    /// CPU usage and throttling
    pub cpu: Option<Cpu>,
    /// Memory usage and limits
    pub memory: Option<Memory>,
    /// Pid count and limits
    pub pids: Option<Pids>,
    /// Disk IO usage and limits
    #[serde(rename = "blkio")]
    pub blk_io: Option<Blkio>,
    /// Huge memory pages
    #[serde(rename = "hugetlb")]
    pub huge_tlb: Option<HashMap<String, HugeTlb>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuUsage {
    /// Total CPU time consumed
    pub total: Option<u64>,
    /// Total CPU time consumed per core
    pub percpu: Option<Vec<u64>>,
    /// Time spent by tasks in kernel mode
    pub kernel: Option<u64>,
    /// Time spent by tasks in user mode
    pub user: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Throttling {
    /// Number of periods with throttling active
    pub periods: Option<u64>,
    /// Number of periods when the container hit its throttling limit
    #[serde(rename = "throttledPeriods")]
    pub throttled_periods: Option<u64>,
    /// Aggregate time the container was throttled for in nanoseconds
    #[serde(rename = "throttledTime")]
    pub throttled_time: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cpu {
    pub usage: Option<CpuUsage>,
    pub throttling: Option<Throttling>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Limit in bytes
    pub limit: Option<u64>,
    /// Usage in bytes
    pub usage: Option<u64>,
    /// Maximum usage in bytes
    pub max: Option<u64>,
    /// Memory allocation failures
    #[serde(rename = "failcnt")]
    pub fail_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    /// Memory used for cache
    pub cache: Option<u64>,
    /// Usage of memory
    pub usage: Option<MemoryEntry>,
    /// Usage of memory + swap
    pub swap: Option<MemoryEntry>,
    /// Usage of kernel memory
    pub kernel: Option<MemoryEntry>,
    /// Usage of kernel TCP memory
    #[serde(rename = "kernelTCP")]
    pub kernel_tcp: Option<MemoryEntry>,
    /// Raw memory statistics
    pub raw: Option<HashMap<String, u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pids {
    /// Number of pids in the cgroup
    pub current: Option<u64>,
    /// Active pids hard limit
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlkioEntry {
    pub major: Option<u64>,
    pub minor: Option<u64>,
    pub op: Option<String>,
    pub value: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blkio {
    /// Number of bytes transferred to and from the disk
    #[serde(rename = "ioServiceBytesRecursive")]
    pub io_service_bytes_recursive: Option<Vec<BlkioEntry>>,
    /// Number of io requests issued to the disk
    #[serde(rename = "ioServicedRecursive")]
    pub io_serviced_recursive: Option<Vec<BlkioEntry>>,
    /// Number of queued disk io requests
    #[serde(rename = "ioQueueRecursive")]
    pub io_queued_recursive: Option<Vec<BlkioEntry>>,
    /// Amount of time io requests took to service
    #[serde(rename = "ioServiceTimeRecursive")]
    pub io_service_time_recursive: Option<Vec<BlkioEntry>>,
    /// Amount of time io requests spent waiting in the queue
    #[serde(rename = "ioWaitTimeRecursive")]
    pub io_wait_time_recursive: Option<Vec<BlkioEntry>>,
    /// Number of merged io requests
    #[serde(rename = "ioMergedRecursive")]
    pub io_merged_recursive: Option<Vec<BlkioEntry>>,
    /// Disk time allocated the device
    #[serde(rename = "ioTimeRecursive")]
    pub io_time_recursive: Option<Vec<BlkioEntry>>,
    /// Number of sectors transferred to and from the io device
    #[serde(rename = "sectorsRecursive")]
    pub sectors_recursive: Option<Vec<BlkioEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HugeTlb {
    /// Current res_counter usage for hugetlb
    pub usage: Option<u64>,
    /// Maximum usage ever recorded
    pub max: Option<u64>,
    /// Number of allocation failures
    #[serde(rename = "failcnt")]
    pub fail_count: Option<u64>,
}
