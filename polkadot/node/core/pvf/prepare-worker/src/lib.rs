// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Contains the logic for preparing PVFs. Used by the polkadot-prepare-worker binary.

mod memory_stats;

use polkadot_node_core_pvf_common::executor_intf::{prepare, prevalidate};

// NOTE: Initializing logging in e.g. tests will not have an effect in the workers, as they are
//       separate spawned processes. Run with e.g. `RUST_LOG=parachain::pvf-prepare-worker=trace`.
const LOG_TARGET: &str = "parachain::pvf-prepare-worker";

#[cfg(target_os = "linux")]
use crate::memory_stats::max_rss_stat::{extract_max_rss_stat, get_max_rss_thread};
#[cfg(any(target_os = "linux", feature = "jemalloc-allocator"))]
use crate::memory_stats::memory_tracker::{get_memory_tracker_loop_stats, memory_tracker_loop};
use libc;
use nix::{
	errno::Errno,
	sys::{
		resource::{Usage, UsageWho},
		wait::WaitStatus,
	},
	unistd::{ForkResult, Pid},
};
use os_pipe::{self, PipeReader, PipeWriter};
use parity_scale_codec::{Decode, Encode};
use polkadot_node_core_pvf_common::{
	error::{PrepareError, PrepareResult},
	executor_intf::create_runtime_from_artifact_bytes,
	framed_recv_blocking, framed_send_blocking,
	prepare::{MemoryStats, PrepareJobKind, PrepareStats},
	pvf::PvfPrepData,
	worker::{
		cpu_time_monitor_loop, run_worker, stringify_panic_payload,
		thread::{self, spawn_worker_thread, WaitOutcome},
		WorkerKind,
	},
	worker_dir, ProcessTime, SecurityStatus,
};
use polkadot_primitives::ExecutorParams;
use std::{
	fs,
	io::{self, Read},
	os::{
		fd::{AsRawFd, RawFd},
		unix::net::UnixStream,
	},
	path::PathBuf,
	process,
	sync::{mpsc::channel, Arc},
	time::Duration,
};
use tracking_allocator::TrackingAllocator;

#[cfg(any(target_os = "linux", feature = "jemalloc-allocator"))]
#[global_allocator]
static ALLOC: TrackingAllocator<tikv_jemallocator::Jemalloc> =
	TrackingAllocator(tikv_jemallocator::Jemalloc);

#[cfg(not(any(target_os = "linux", feature = "jemalloc-allocator")))]
#[global_allocator]
static ALLOC: TrackingAllocator<std::alloc::System> = TrackingAllocator(std::alloc::System);

/// Contains the bytes for a successfully compiled artifact.
#[derive(Encode, Decode)]
pub struct CompiledArtifact(Vec<u8>);

impl CompiledArtifact {
	/// Creates a `CompiledArtifact`.
	pub fn new(code: Vec<u8>) -> Self {
		Self(code)
	}
}

impl AsRef<[u8]> for CompiledArtifact {
	fn as_ref(&self) -> &[u8] {
		self.0.as_slice()
	}
}

/// Get a worker request.
fn recv_request(stream: &mut UnixStream) -> io::Result<PvfPrepData> {
	let pvf = framed_recv_blocking(stream)?;
	let pvf = PvfPrepData::decode(&mut &pvf[..]).map_err(|e| {
		io::Error::new(
			io::ErrorKind::Other,
			format!("prepare pvf recv_request: failed to decode PvfPrepData: {}", e),
		)
	})?;
	Ok(pvf)
}

/// Send a worker response.
fn send_response(stream: &mut UnixStream, result: PrepareResult) -> io::Result<()> {
	framed_send_blocking(stream, &result.encode())
}

fn start_memory_tracking(fd: RawFd, limit: Option<isize>) {
	unsafe {
		// SAFETY: Inside the failure handler, the allocator is locked and no allocations or
		// deallocations are possible. For Linux, that always holds for the code below, so it's
		// safe. For MacOS, that technically holds at the time of writing, but there are no future
		// guarantees.
		// The arguments of unsafe `libc` calls are valid, the payload validity is covered with
		// a test.
		ALLOC.start_tracking(
			limit,
			Some(Box::new(move || {
				#[cfg(target_os = "linux")]
				{
					// Syscalls never allocate or deallocate, so this is safe.
					libc::syscall(libc::SYS_write, fd, OOM_PAYLOAD.as_ptr(), OOM_PAYLOAD.len());
					libc::syscall(libc::SYS_close, fd);
					// Make sure we exit from all threads. Copied from glibc.
					libc::syscall(libc::SYS_exit_group, 1);
					loop {
						libc::syscall(libc::SYS_exit, 1);
					}
				}
				#[cfg(not(target_os = "linux"))]
				{
					// Syscalls are not available on MacOS, so we have to use `libc` wrappers.
					// Technically, there may be allocations inside, although they shouldn't be
					// there. In that case, we'll see deadlocks on MacOS after the OOM condition
					// triggered. As we consider running a validator on MacOS unsafe, and this
					// code is only run by a validator, it's a lesser evil.
					libc::write(fd, OOM_PAYLOAD.as_ptr().cast(), OOM_PAYLOAD.len());
					libc::close(fd);
					libc::_exit(1);
				}
			})),
		);
	}
}

fn end_memory_tracking() -> isize {
	ALLOC.end_tracking()
}

/// The entrypoint that the spawned prepare worker should start with.
///
/// # Parameters
///
/// - `socket_path`: specifies the path to the socket used to communicate with the host.
///
/// - `worker_dir_path`: specifies the path to the worker-specific temporary directory.
///
/// - `node_version`: if `Some`, is checked against the `worker_version`. A mismatch results in
///   immediate worker termination. `None` is used for tests and in other situations when version
///   check is not necessary.
///
/// - `worker_version`: see above
///
/// - `security_status`: contains the detected status of security features.
///
/// # Flow
///
/// This runs the following in a loop:
///
/// 1. Get the code and parameters for preparation from the host.
///
/// 2. Start a new child process
///
/// 3. Start the memory tracker and the actual preparation in two separate threads.
///
/// 4. Wait on the two threads created in step 3.
///
/// 5. Stop the memory tracker and get the stats.
///
/// 6. Pipe the result back to the parent process and exit from child process.
///
/// 7. If compilation succeeded, write the compiled artifact into a temporary file.
///
/// 8. Send the result of preparation back to the host. If any error occurred in the above steps, we
///    send that in the `PrepareResult`.
pub fn worker_entrypoint(
	socket_path: PathBuf,
	worker_dir_path: PathBuf,
	node_version: Option<&str>,
	worker_version: Option<&str>,
	security_status: SecurityStatus,
) {
	run_worker(
		WorkerKind::Prepare,
		socket_path,
		worker_dir_path,
		node_version,
		worker_version,
		&security_status,
		|mut stream, worker_dir_path| {
			let worker_pid = process::id();
			let temp_artifact_dest = worker_dir::prepare_tmp_artifact(&worker_dir_path);

			loop {
				let pvf = recv_request(&mut stream)?;
				gum::debug!(
					target: LOG_TARGET,
					%worker_pid,
					"worker: preparing artifact",
				);

				let preparation_timeout = pvf.prep_timeout();
				let prepare_job_kind = pvf.prep_kind();
				let executor_params = pvf.executor_params();

				let (pipe_reader, pipe_writer) = os_pipe::pipe()?;

				let usage_before = match nix::sys::resource::getrusage(UsageWho::RUSAGE_CHILDREN) {
					Ok(usage) => usage,
					Err(errno) => {
						let result = Err(error_from_errno("getrusage before", errno));
						send_response(&mut stream, result)?;
						continue
					},
				};

				// SAFETY: new process is spawned within a single threaded process. This invariant
				// is enforced by tests.
				let result = match unsafe { nix::unistd::fork() } {
					Err(errno) => Err(error_from_errno("fork", errno)),
					Ok(ForkResult::Child) => {
						// Dropping the stream closes the underlying socket. We want to make sure
						// that the sandboxed child can't get any kind of information from the
						// outside world. The only IPC it should be able to do is sending its
						// response over the pipe.
						drop(stream);
						// Drop the read end so we don't have too many FDs open.
						drop(pipe_reader);

						handle_child_process(
							pvf,
							pipe_writer,
							preparation_timeout,
							prepare_job_kind,
							executor_params,
						)
					},
					Ok(ForkResult::Parent { child }) => {
						// the read end will wait until all write ends have been closed,
						// this drop is necessary to avoid deadlock
						drop(pipe_writer);

						handle_parent_process(
							pipe_reader,
							child,
							temp_artifact_dest.clone(),
							worker_pid,
							usage_before,
							preparation_timeout,
						)
					},
				};

				gum::trace!(
					target: LOG_TARGET,
					%worker_pid,
					"worker: sending result to host: {:?}",
					result
				);
				send_response(&mut stream, result)?;
			}
		},
	);
}

fn prepare_artifact(pvf: PvfPrepData) -> Result<CompiledArtifact, PrepareError> {
	let blob = match prevalidate(&pvf.code()) {
		Err(err) => return Err(PrepareError::Prevalidation(format!("{:?}", err))),
		Ok(b) => b,
	};

	match prepare(blob, &pvf.executor_params()) {
		Ok(compiled_artifact) => Ok(CompiledArtifact::new(compiled_artifact)),
		Err(err) => Err(PrepareError::Preparation(format!("{:?}", err))),
	}
}

/// Try constructing the runtime to catch any instantiation errors during pre-checking.
fn runtime_construction_check(
	artifact_bytes: &[u8],
	executor_params: &ExecutorParams,
) -> Result<(), PrepareError> {
	// SAFETY: We just compiled this artifact.
	let result = unsafe { create_runtime_from_artifact_bytes(artifact_bytes, executor_params) };
	result
		.map(|_runtime| ())
		.map_err(|err| PrepareError::RuntimeConstruction(format!("{:?}", err)))
}

#[derive(Encode, Decode)]
struct JobResponse {
	artifact: CompiledArtifact,
	memory_stats: MemoryStats,
}

/// This is used to handle child process during pvf prepare worker.
/// It prepares the artifact and tracks memory stats during preparation
/// and pipes back the response to the parent process
///
/// # Arguments
///
/// - `pvf`: `PvfPrepData` structure, containing data to prepare the artifact
///
/// - `pipe_write`: A `PipeWriter` structure, the writing end of a pipe.
///
/// - `preparation_timeout`: The timeout in `Duration`.
///
/// - `prepare_job_kind`: The kind of prepare job.
///
/// - `executor_params`: Deterministically serialized execution environment semantics.
///
/// # Returns
///
/// - If any error occur, pipe response back with `PrepareError`.
///
/// - If success, pipe back `JobResponse`.
fn handle_child_process(
	pvf: PvfPrepData,
	mut pipe_write: PipeWriter,
	preparation_timeout: Duration,
	prepare_job_kind: PrepareJobKind,
	executor_params: Arc<ExecutorParams>,
) -> ! {
	let worker_job_pid = process::id();
	gum::debug!(
		target: LOG_TARGET,
		%worker_job_pid,
		?prepare_job_kind,
		?preparation_timeout,
		"worker job: preparing artifact",
	);

	// Conditional variable to notify us when a thread is done.
	let condvar = thread::get_condvar();

	// Run the memory tracker in a regular, non-worker thread.
	#[cfg(any(target_os = "linux", feature = "jemalloc-allocator"))]
	let condvar_memory = Arc::clone(&condvar);
	#[cfg(any(target_os = "linux", feature = "jemalloc-allocator"))]
	let memory_tracker_thread = std::thread::spawn(|| memory_tracker_loop(condvar_memory));

	start_memory_tracking(
		pipe_write.as_raw_fd(),
		executor_params.prechecking_max_memory().map(|v| {
			v.try_into().unwrap_or_else(|_| {
				gum::warn!(
					LOG_TARGET,
					%worker_job_pid,
					"Illegal pre-checking max memory value {} discarded",
					v,
				);
				0
			})
		}),
	);

	let cpu_time_start = ProcessTime::now();

	// Spawn a new thread that runs the CPU time monitor.
	let (cpu_time_monitor_tx, cpu_time_monitor_rx) = channel::<()>();
	let cpu_time_monitor_thread = thread::spawn_worker_thread(
		"cpu time monitor thread",
		move || cpu_time_monitor_loop(cpu_time_start, preparation_timeout, cpu_time_monitor_rx),
		Arc::clone(&condvar),
		WaitOutcome::TimedOut,
	)
	.unwrap_or_else(|err| {
		send_child_response(&mut pipe_write, Err(PrepareError::IoErr(err.to_string())))
	});

	let prepare_thread = spawn_worker_thread(
		"prepare worker",
		move || {
			#[allow(unused_mut)]
			let mut result = prepare_artifact(pvf);

			// Get the `ru_maxrss` stat. If supported, call getrusage for the thread.
			#[cfg(target_os = "linux")]
			let mut result = result.map(|artifact| (artifact, get_max_rss_thread()));

			// If we are pre-checking, check for runtime construction errors.
			//
			// As pre-checking is more strict than just preparation in terms of memory
			// and time, it is okay to do extra checks here. This takes negligible time
			// anyway.
			if let PrepareJobKind::Prechecking = prepare_job_kind {
				result = result.and_then(|output| {
					runtime_construction_check(output.0.as_ref(), &executor_params)?;
					Ok(output)
				});
			}
			result
		},
		Arc::clone(&condvar),
		WaitOutcome::Finished,
	)
	.unwrap_or_else(|err| {
		send_child_response(&mut pipe_write, Err(PrepareError::IoErr(err.to_string())))
	});

	let outcome = thread::wait_for_threads(condvar);

	let peak_alloc = {
		let peak = end_memory_tracking();
		gum::debug!(
			target: LOG_TARGET,
			%worker_job_pid,
			"prepare job peak allocation is {} bytes",
			peak,
		);
		peak
	};

	let result = match outcome {
		WaitOutcome::Finished => {
			let _ = cpu_time_monitor_tx.send(());

			match prepare_thread.join().unwrap_or_else(|err| {
				send_child_response(
					&mut pipe_write,
					Err(PrepareError::JobError(stringify_panic_payload(err))),
				)
			}) {
				Err(err) => Err(err),
				Ok(ok) => {
					cfg_if::cfg_if! {
					if #[cfg(target_os = "linux")] {
						let (artifact, max_rss) = ok;
					} else {
						let artifact = ok;
					}
					}

					// Stop the memory stats worker and get its observed memory stats.
					#[cfg(any(target_os = "linux", feature = "jemalloc-allocator"))]
					let memory_tracker_stats = get_memory_tracker_loop_stats(memory_tracker_thread, process::id());

					let memory_stats = MemoryStats {
						#[cfg(any(target_os = "linux", feature = "jemalloc-allocator"))]
						memory_tracker_stats,
						#[cfg(target_os = "linux")]
						max_rss: extract_max_rss_stat(max_rss, process::id()),
						// Negative peak allocation values are legit; they are narrow
						// corner cases and shouldn't affect overall statistics
						// significantly
						peak_tracked_alloc: if peak_alloc > 0 { peak_alloc as u64 } else { 0u64 },
					};

					Ok(JobResponse { artifact, memory_stats })
				},
			}
		},

		// If the CPU thread is not selected, we signal it to end, the join handle is
		// dropped and the thread will finish in the background.
		WaitOutcome::TimedOut => match cpu_time_monitor_thread.join() {
			Ok(Some(_cpu_time_elapsed)) => Err(PrepareError::TimedOut),
			Ok(None) => Err(PrepareError::IoErr("error communicating over closed channel".into())),
			Err(err) => Err(PrepareError::IoErr(stringify_panic_payload(err))),
		},
		WaitOutcome::Pending =>
			unreachable!("we run wait_while until the outcome is no longer pending; qed"),
	};

	send_child_response(&mut pipe_write, result);
}

/// Waits for child process to finish and handle child response from pipe.
///
/// # Arguments
///
/// - `pipe_read`: A `PipeReader` used to read data from the child process.
///
/// - `child`: The child pid.
///
/// - `temp_artifact_dest`: The destination `PathBuf` to write the temporary artifact file.
///
/// - `worker_pid`: The PID of the child process.
///
/// - `usage_before`: Resource usage statistics before executing the child process.
///
/// - `timeout`: The maximum allowed time for the child process to finish, in `Duration`.
///
/// # Returns
///
/// - If the child send response without an error, this function returns `Ok(PrepareStats)`
///   containing memory and CPU usage statistics.
///
/// - If the child send response with an error, it returns a `PrepareError` with that error.
///
/// - If the child process timeout, it returns `PrepareError::TimedOut`.
fn handle_parent_process(
	mut pipe_read: PipeReader,
	child: Pid,
	temp_artifact_dest: PathBuf,
	worker_pid: u32,
	usage_before: Usage,
	timeout: Duration,
) -> Result<PrepareStats, PrepareError> {
	// Read from the child. Don't decode unless the process exited normally, which we check later.
	let mut received_data = Vec::new();
	pipe_read
		.read_to_end(&mut received_data)
		.map_err(|err| PrepareError::IoErr(err.to_string()))?;

	let status = nix::sys::wait::waitpid(child, None);
	gum::trace!(
		target: LOG_TARGET,
		%worker_pid,
		"prepare worker received wait status from job: {:?}",
		status,
	);

	let usage_after = nix::sys::resource::getrusage(UsageWho::RUSAGE_CHILDREN)
		.map_err(|errno| error_from_errno("getrusage after", errno))?;

	// Using `getrusage` is needed to check whether child has timedout since we cannot rely on
	// child to report its own time.
	// As `getrusage` returns resource usage from all terminated child processes,
	// it is necessary to subtract the usage before the current child process to isolate its cpu
	// time
	let cpu_tv = get_total_cpu_usage(usage_after) - get_total_cpu_usage(usage_before);
	if cpu_tv >= timeout {
		gum::warn!(
			target: LOG_TARGET,
			%worker_pid,
			"prepare job took {}ms cpu time, exceeded prepare timeout {}ms",
			cpu_tv.as_millis(),
			timeout.as_millis(),
		);
		return Err(PrepareError::TimedOut)
	}

	match status {
		Ok(WaitStatus::Exited(_pid, exit_status)) => {
			let mut reader = io::BufReader::new(received_data.as_slice());
			let result = recv_child_response(&mut reader)
				.map_err(|err| PrepareError::JobError(err.to_string()))?;

			match result {
				Err(err) => Err(err),
				Ok(response) => {
					// The exit status should have been zero if no error occurred.
					if exit_status != 0 {
						return Err(PrepareError::JobError(format!(
							"unexpected exit status: {}",
							exit_status
						)))
					}

					// Write the serialized artifact into a temp file.
					//
					// PVF host only keeps artifacts statuses in its memory,
					// successfully compiled code gets stored on the disk (and
					// consequently deserialized by execute-workers). The prepare worker
					// is only required to send `Ok` to the pool to indicate the
					// success.
					gum::debug!(
						target: LOG_TARGET,
						%worker_pid,
						"worker: writing artifact to {}",
						temp_artifact_dest.display(),
					);
					// Write to the temp file created by the host.
					if let Err(err) = fs::write(&temp_artifact_dest, &response.artifact) {
						return Err(PrepareError::IoErr(err.to_string()))
					};

					Ok(PrepareStats {
						memory_stats: response.memory_stats,
						cpu_time_elapsed: cpu_tv,
					})
				},
			}
		},
		// The job was killed by the given signal.
		//
		// The job gets SIGSYS on seccomp violations, but this signal may have been sent for some
		// other reason, so we still need to check for seccomp violations elsewhere.
		Ok(WaitStatus::Signaled(_pid, signal, _core_dump)) =>
			Err(PrepareError::JobDied(format!("received signal: {signal:?}"))),
		Err(errno) => Err(error_from_errno("waitpid", errno)),

		// An attacker can make the child process return any exit status it wants. So we can treat
		// all unexpected cases the same way.
		Ok(unexpected_wait_status) => Err(PrepareError::JobDied(format!(
			"unexpected status from wait: {unexpected_wait_status:?}"
		))),
	}
}

/// Calculate the total CPU time from the given `usage` structure, returned from
/// [`nix::sys::resource::getrusage`], and calculates the total CPU time spent, including both user
/// and system time.
///
/// # Arguments
///
/// - `rusage`: Contains resource usage information.
///
/// # Returns
///
/// Returns a `Duration` representing the total CPU time.
fn get_total_cpu_usage(rusage: Usage) -> Duration {
	let micros = (((rusage.user_time().tv_sec() + rusage.system_time().tv_sec()) * 1_000_000) +
		(rusage.system_time().tv_usec() + rusage.user_time().tv_usec()) as i64) as u64;

	return Duration::from_micros(micros)
}

/// Get a job response.
fn recv_child_response(received_data: &mut io::BufReader<&[u8]>) -> io::Result<JobResult> {
	let response_bytes = framed_recv_blocking(received_data)?;
	JobResult::decode(&mut response_bytes.as_slice()).map_err(|e| {
		io::Error::new(
			io::ErrorKind::Other,
			format!("prepare pvf recv_child_response: decode error: {:?}", e),
		)
	})
}

/// Write a job response to the pipe and exit process after.
///
/// # Arguments
///
/// - `pipe_write`: A `PipeWriter` structure, the writing end of a pipe.
///
/// - `response`: Child process response
fn send_child_response(pipe_write: &mut PipeWriter, response: JobResult) -> ! {
	framed_send_blocking(pipe_write, response.encode().as_slice())
		.unwrap_or_else(|_| process::exit(libc::EXIT_FAILURE));

	if response.is_ok() {
		process::exit(libc::EXIT_SUCCESS)
	} else {
		process::exit(libc::EXIT_FAILURE)
	}
}

fn error_from_errno(context: &'static str, errno: Errno) -> PrepareError {
	PrepareError::Kernel(format!("{}: {}: {}", context, errno, io::Error::last_os_error()))
}

type JobResult = Result<JobResponse, PrepareError>;

/// Pre-encoded length-prefixed `Result::Err(PrepareError::OutOfMemory)`
const OOM_PAYLOAD: &[u8] = b"\x02\x00\x00\x00\x00\x00\x00\x00\x01\x08";

#[test]
fn pre_encoded_payloads() {
	// NOTE: This must match the type of `response` in `send_child_response`.
	let oom_unencoded: JobResult = Result::Err(PrepareError::OutOfMemory);
	let oom_encoded = oom_unencoded.encode();
	// The payload is prefixed with	its length in `framed_send`.
	let mut oom_payload = oom_encoded.len().to_le_bytes().to_vec();
	oom_payload.extend(oom_encoded);
	assert_eq!(oom_payload, OOM_PAYLOAD);
}
