use std::cell::Cell;
use std::ffi::{CStr, CString, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::mpsc::SyncSender;
use std::time::Instant;

use libpulse_sys as pulse;

use crate::status::{Update, Volume};
use crate::watch::{self, TIMEOUT};

fn percent(raw: u32) -> u16 {
    let normal = u64::from(pulse::PA_VOLUME_NORM);
    ((u64::from(raw) * 100 + normal / 2) / normal).min(999) as u16
}

struct Mainloop(NonNull<pulse::pa_mainloop>);

impl Drop for Mainloop {
    fn drop(&mut self) {
        // SAFETY: This owns the mainloop, and Pulse drops its context before this field.
        unsafe { pulse::pa_mainloop_free(self.0.as_ptr()) };
    }
}

// Raw pointers keep these owners single-threaded. No callbacks borrow Pulse itself.
struct Pulse {
    context: NonNull<pulse::pa_context>,
    mainloop: Mainloop,
}

impl Drop for Pulse {
    fn drop(&mut self) {
        // SAFETY: The context is live and exclusively owned. Unregister userdata before
        // disconnecting; the mainloop remains alive until after this destructor returns.
        unsafe {
            pulse::pa_context_set_subscribe_callback(self.context.as_ptr(), None, ptr::null_mut());
            pulse::pa_context_disconnect(self.context.as_ptr());
            pulse::pa_context_unref(self.context.as_ptr());
        }
    }
}

// The request state cannot be accessed or dropped until cancellation/unref has
// disabled callbacks. Unlike the high-level binding, no callback allocation is owned by C.
struct Operation<'a, T> {
    ptr: NonNull<pulse::pa_operation>,
    _state: PhantomData<&'a mut T>,
}

impl<'a, T> Operation<'a, T> {
    fn start(
        state: &'a mut T,
        create: impl FnOnce(*mut c_void) -> *mut pulse::pa_operation,
    ) -> Result<Self, String> {
        let ptr = NonNull::new(create(ptr::from_mut(state).cast()))
            .ok_or("cannot start pulse request")?;
        Ok(Self {
            ptr,
            _state: PhantomData,
        })
    }

    fn state(&self) -> pulse::pa_operation_state_t {
        // SAFETY: The operation reference is owned until Drop.
        unsafe { pulse::pa_operation_get_state(self.ptr.as_ptr()) }
    }
}

impl<T> Drop for Operation<'_, T> {
    fn drop(&mut self) {
        // SAFETY: This owns one operation reference. Cancellation guarantees no more
        // callbacks, including on error paths, before the borrowed state is released.
        unsafe {
            pulse::pa_operation_cancel(self.ptr.as_ptr());
            pulse::pa_operation_unref(self.ptr.as_ptr());
        }
    }
}

type ServerReply = Result<Option<CString>, &'static str>;

extern "C" fn server_reply(
    _: *mut pulse::pa_context,
    info: *const pulse::pa_server_info,
    userdata: *mut c_void,
) {
    if userdata.is_null() {
        return;
    }
    // Allocation/copying stays inside catch_unwind so no Rust panic crosses C.
    let result = std::panic::catch_unwind(|| {
        if info.is_null() {
            return Err("pulse server query failed");
        }
        // SAFETY: Pulse supplies a valid server-info object for this callback.
        let name = unsafe { (*info).default_sink_name };
        if name.is_null() {
            Ok(None)
        } else {
            // SAFETY: Pulse's name is NUL terminated and valid throughout the callback.
            // Preserve its bytes rather than changing non-UTF-8 sink names.
            Ok(Some(unsafe { CStr::from_ptr(name) }.to_owned()))
        }
    });
    // SAFETY: Operation borrows this caller-owned ServerReply until callbacks are
    // cancelled. It is not otherwise accessed while the mainloop dispatches.
    unsafe {
        *userdata.cast::<ServerReply>() = result.unwrap_or(Err("pulse callback panicked"));
    }
}

#[derive(Default)]
struct SinkReply {
    volume: Option<Volume>,
    complete: bool,
    error: Option<i32>,
}

impl SinkReply {
    fn result(self) -> Result<Option<Volume>, String> {
        match self.error {
            // A sink can disappear between the server and sink queries.
            Some(code) if code == pulse::PA_ERR_NOENTITY as i32 => Ok(None),
            Some(code) => Err(format!("pulse sink query failed ({code})")),
            None if self.complete => Ok(self.volume),
            None => Err("incomplete pulse sink reply".into()),
        }
    }
}

extern "C" fn sink_reply(
    context: *mut pulse::pa_context,
    info: *const pulse::pa_sink_info,
    eol: i32,
    userdata: *mut c_void,
) {
    if userdata.is_null() {
        return;
    }
    // SAFETY: Operation keeps this SinkReply alive and unborrowed during dispatch.
    let reply = unsafe { &mut *userdata.cast::<SinkReply>() };
    if eol < 0 {
        reply.error = Some(if context.is_null() {
            pulse::PA_ERR_UNKNOWN as i32
        } else {
            // SAFETY: Pulse supplies its live context, checked non-null above.
            unsafe { pulse::pa_context_errno(context) }
        });
    } else if eol > 0 {
        reply.complete = true;
    } else if info.is_null() {
        reply.error = Some(pulse::PA_ERR_NODATA as i32);
    } else {
        // SAFETY: Pulse supplies a valid sink-info object for this item callback.
        let info = unsafe { &*info };
        let channels = usize::from(info.volume.channels);
        if channels == 0 || channels > info.volume.values.len() {
            reply.error = Some(pulse::PA_ERR_INVALID as i32);
            return;
        }
        // Bounds were checked above; arithmetic uses u64 to avoid overflow.
        let sum: u64 = info.volume.values[..channels]
            .iter()
            .map(|&value| u64::from(value))
            .sum();
        reply.volume = Some(Volume {
            percent: percent((sum / channels as u64) as u32),
            muted: info.mute != 0,
        });
    }
    // This callback only does checked arithmetic and primitive assignments: it cannot unwind.
}

extern "C" fn subscription_reply(_: *mut pulse::pa_context, ok: i32, userdata: *mut c_void) {
    if !userdata.is_null() {
        // SAFETY: Operation borrows this bool until callbacks are cancelled. No panicking work.
        unsafe { *userdata.cast::<bool>() = ok != 0 };
    }
}

extern "C" fn changed(
    _: *mut pulse::pa_context,
    event: pulse::pa_subscription_event_type_t,
    _: u32,
    userdata: *mut c_void,
) {
    let facility = event & pulse::PA_SUBSCRIPTION_EVENT_FACILITY_MASK;
    if !userdata.is_null()
        && matches!(
            facility,
            pulse::PA_SUBSCRIPTION_EVENT_SINK | pulse::PA_SUBSCRIPTION_EVENT_SERVER
        )
    {
        // SAFETY: run declares this Cell before Pulse and unregisters on Pulse::drop.
        // Only shared references exist and Cell::set cannot panic.
        unsafe { &*userdata.cast::<Cell<bool>>() }.set(true);
    }
}

impl Pulse {
    fn state(&self) -> pulse::pa_context_state_t {
        // SAFETY: The context is live for the lifetime of self.
        unsafe { pulse::pa_context_get_state(self.context.as_ptr()) }
    }

    fn step(&self, deadline: Instant) -> Result<(), String> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("pulse request timed out")?;
        let timeout = remaining.as_micros().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: This single thread owns the mainloop. Callback userdata is separate
        // caller-owned state, never an alias of this shared Pulse reference.
        let ok = unsafe {
            pulse::pa_mainloop_prepare(self.mainloop.0.as_ptr(), timeout) >= 0
                && pulse::pa_mainloop_poll(self.mainloop.0.as_ptr()) >= 0
                && pulse::pa_mainloop_dispatch(self.mainloop.0.as_ptr()) >= 0
        };
        if !ok {
            return Err("pulse mainloop failed".into());
        }
        if matches!(
            self.state(),
            pulse::PA_CONTEXT_FAILED | pulse::PA_CONTEXT_TERMINATED
        ) {
            return Err("pulse disconnected".into());
        }
        Ok(())
    }

    fn wait<T>(&self, operation: Operation<'_, T>) -> Result<(), String> {
        let deadline = Instant::now() + TIMEOUT;
        while operation.state() == pulse::PA_OPERATION_RUNNING {
            self.step(deadline)?;
        }
        if operation.state() != pulse::PA_OPERATION_DONE {
            return Err("pulse operation cancelled".into());
        }
        Ok(())
    }

    fn connect() -> Result<Self, String> {
        // SAFETY: Creates a new, independently owned mainloop; null is checked.
        let mainloop = Mainloop(
            NonNull::new(unsafe { pulse::pa_mainloop_new() })
                .ok_or("cannot create pulse mainloop")?,
        );
        // SAFETY: mainloop is live and its API pointer is borrowed, not owned.
        let api = unsafe { pulse::pa_mainloop_get_api(mainloop.0.as_ptr()) };
        if api.is_null() {
            return Err("cannot get pulse mainloop API".into());
        }
        // SAFETY: API and NUL-terminated application name are valid for this call.
        let context = NonNull::new(unsafe { pulse::pa_context_new(api, c"tinystatus".as_ptr()) })
            .ok_or("cannot create pulse context")?;
        let pulse = Self { context, mainloop };
        // SAFETY: Context is live; null selects the default server and spawn API.
        if unsafe {
            pulse::pa_context_connect(
                pulse.context.as_ptr(),
                ptr::null(),
                pulse::PA_CONTEXT_NOAUTOSPAWN,
                ptr::null(),
            )
        } < 0
        {
            return Err("cannot connect pulse context".into());
        }
        let deadline = Instant::now() + TIMEOUT;
        while pulse.state() != pulse::PA_CONTEXT_READY {
            pulse.step(deadline)?;
        }
        Ok(pulse)
    }

    fn volume(&self) -> Result<Option<Volume>, String> {
        let context = self.context.as_ptr();
        let mut name: ServerReply = Err("missing pulse server reply");
        let operation = Operation::start(&mut name, |userdata| {
            // SAFETY: Context is live; Operation keeps name valid until cancellation.
            unsafe { pulse::pa_context_get_server_info(context, Some(server_reply), userdata) }
        })?;
        self.wait(operation)?;
        let Some(name) = name? else {
            return Ok(None);
        };
        let mut reply = SinkReply::default();
        let operation = Operation::start(&mut reply, |userdata| {
            // SAFETY: Context and CString are live. Pulse copies the name during this
            // call; Operation keeps reply valid through all item/EOL callbacks.
            unsafe {
                pulse::pa_context_get_sink_info_by_name(
                    context,
                    name.as_ptr(),
                    Some(sink_reply),
                    userdata,
                )
            }
        })?;
        self.wait(operation)?;
        reply.result()
    }
}

pub fn current() -> Result<Option<Volume>, String> {
    Pulse::connect()?.volume()
}

fn run(tx: &SyncSender<Update>) -> Result<(), String> {
    // Drop order is important: Pulse unregisters its callback before dirty is dropped.
    let dirty = Cell::new(false);
    let pulse = Pulse::connect()?;
    let context = pulse.context.as_ptr();
    // SAFETY: dirty has a stable stack address until after Pulse unregisters this
    // callback. Callbacks only take shared references to the Cell, on this thread.
    unsafe {
        pulse::pa_context_set_subscribe_callback(
            context,
            Some(changed),
            ptr::from_ref(&dirty).cast_mut().cast(),
        );
    }
    let mut subscribed = false;
    let operation = Operation::start(&mut subscribed, |userdata| {
        // SAFETY: Context is live; Operation keeps subscribed valid until cancellation.
        unsafe {
            pulse::pa_context_subscribe(
                context,
                pulse::PA_SUBSCRIPTION_MASK_SINK | pulse::PA_SUBSCRIPTION_MASK_SERVER,
                Some(subscription_reply),
                userdata,
            )
        }
    })?;
    pulse.wait(operation)?;
    if !subscribed {
        return Err("pulse subscription failed".into());
    }
    let mut previous = None;
    loop {
        // Clear before querying so events dispatched during a query stay pending.
        dirty.set(false);
        let volume = pulse.volume()?;
        if previous != Some(volume) {
            tx.send(Update::Volume(volume))
                .map_err(|_| "output closed")?;
            previous = Some(volume);
        }
        while !dirty.get() {
            // SAFETY: The mainloop is owned on this thread. All callback userdata
            // remains live, and null retval means its optional exit code is ignored.
            let result = unsafe {
                pulse::pa_mainloop_iterate(pulse.mainloop.0.as_ptr(), 1, ptr::null_mut())
            };
            if result < 0 || pulse.state() != pulse::PA_CONTEXT_READY {
                return Err("pulse disconnected".into());
            }
        }
    }
}

pub fn watch(tx: SyncSender<Update>) {
    watch::watch(tx, "pulse", || Update::Volume(None), run);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_and_caps_without_overflow() {
        assert_eq!(percent(0), 0);
        assert_eq!(percent(32_768), 50);
        assert_eq!(percent(65_536), 100);
        assert_eq!(percent(u32::MAX), 999);
    }

    #[test]
    fn server_callback_preserves_bytes_and_handles_absence() {
        // SAFETY: pa_server_info contains only integers, raw pointers and C structs
        // whose zero representations are valid; the callback reads only default_sink_name.
        let mut info: pulse::pa_server_info = unsafe { std::mem::zeroed() };
        let mut reply: ServerReply = Err("missing");
        let userdata = ptr::from_mut(&mut reply).cast();
        server_reply(ptr::null_mut(), &info, userdata);
        assert_eq!(reply, Ok(None));
        let name = c"sink-\xff";
        info.default_sink_name = name.as_ptr();
        server_reply(ptr::null_mut(), &info, userdata);
        assert_eq!(reply.as_ref().unwrap().as_deref(), Some(name));
        server_reply(ptr::null_mut(), ptr::null(), userdata);
        assert!(reply.is_err());
    }

    #[test]
    fn sink_callback_handles_items_end_errors_and_invalid_channels() {
        // SAFETY: pa_sink_info consists of C numeric fields, raw pointers and structs
        // with valid zero representations. Only volume/mute are read by this callback.
        let mut info: pulse::pa_sink_info = unsafe { std::mem::zeroed() };
        info.volume.channels = 2;
        info.volume.values[0] = pulse::PA_VOLUME_NORM;
        info.volume.values[1] = 0;
        info.mute = 1;
        let mut reply = SinkReply::default();
        let userdata = ptr::from_mut(&mut reply).cast();
        sink_reply(ptr::null_mut(), &info, 0, userdata);
        sink_reply(ptr::null_mut(), ptr::null(), 1, userdata);
        let volume = reply.result().unwrap().unwrap();
        assert_eq!(volume.percent, 50);
        assert!(volume.muted);

        let mut reply = SinkReply::default();
        sink_reply(
            ptr::null_mut(),
            ptr::null(),
            -1,
            ptr::from_mut(&mut reply).cast(),
        );
        assert!(reply.result().is_err());
        for channels in [0, u8::MAX] {
            info.volume.channels = channels;
            let mut reply = SinkReply::default();
            sink_reply(ptr::null_mut(), &info, 0, ptr::from_mut(&mut reply).cast());
            assert!(reply.result().is_err());
        }
        assert_eq!(
            SinkReply {
                error: Some(pulse::PA_ERR_NOENTITY as i32),
                ..SinkReply::default()
            }
            .result(),
            Ok(None)
        );
        assert!(SinkReply::default().result().is_err());
    }

    #[test]
    fn subscription_callbacks_coalesce_relevant_changes() {
        let dirty = Cell::new(false);
        let userdata = ptr::from_ref(&dirty).cast_mut().cast();
        changed(
            ptr::null_mut(),
            pulse::PA_SUBSCRIPTION_EVENT_SOURCE,
            0,
            userdata,
        );
        assert!(!dirty.get());
        changed(
            ptr::null_mut(),
            pulse::PA_SUBSCRIPTION_EVENT_SERVER,
            0,
            userdata,
        );
        assert!(dirty.replace(false));
        changed(
            ptr::null_mut(),
            pulse::PA_SUBSCRIPTION_EVENT_SINK | pulse::PA_SUBSCRIPTION_EVENT_REMOVE,
            0,
            userdata,
        );
        assert!(dirty.get());
        let mut subscribed = false;
        subscription_reply(ptr::null_mut(), 1, ptr::from_mut(&mut subscribed).cast());
        assert!(subscribed);
    }
}
