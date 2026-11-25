//! Linux: CEF's C API resolved at run time from a `libcef.so` opened with
//! `dlopen`, so neither `dreamer` nor the helper links it. Chromium is the
//! Dreamer Web component, installed apart from the app, and the library is
//! loaded from wherever that is ([`load`]), as the framework is on macOS.
//!
//! The `cef` crate calls CEF's C functions by name. Each of those names is
//! defined here as a two-instruction trampoline that jumps through a slot
//! of [`TABLE`]. Until [`load`] has filled the table, every slot holds
//! [`unloaded`], which aborts with a message instead of jumping into
//! nothing. A trampoline does not touch the arguments or the stack, so one
//! shape fits every signature. This is what CEF's own loader for macOS
//! (`libcef_dll_dylib.cc`, which CEF builds only for macOS) does, with the
//! table filled by `dlsym`.
//!
//! The names are every function the `cef-dll-sys` bindings declare for
//! Linux at the pinned release (they are all exported by that release's
//! `libcef.so`). The build never links `libcef.so` (the workspace's
//! `.cargo/config.toml` replaces `cef-dll-sys`'s build script on Linux), so
//! a function the `cef` crate calls that is missing here fails the link
//! with its name, rather than silently linking the library again.

use std::ffi::{CString, c_void};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

#[cfg(not(target_arch = "x86_64"))]
compile_error!("CEF's run-time loader is written for x86-64 Linux only");

/// Called through a slot that [`load`] has not filled.
extern "C" fn unloaded() {
    eprintln!("dreamer: a CEF function was called before libcef.so was loaded");
    std::process::abort();
}

macro_rules! entry_points {
    ($($name:ident)*) => {
        /// The slot of each entry point in [`TABLE`].
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        #[repr(usize)]
        enum Slot {
            $($name,)*
            Count,
        }

        /// Each entry point's name, NUL-terminated, in slot order.
        const NAMES: [&str; Slot::Count as usize] = [$(concat!(stringify!($name), "\0"),)*];

        $(
            #[unsafe(no_mangle)]
            #[unsafe(naked)]
            unsafe extern "C" fn $name() {
                core::arch::naked_asm!(
                    "jmp qword ptr [rip + {table} + {offset}]",
                    table = sym TABLE,
                    offset = const (Slot::$name as usize) * size_of::<usize>(),
                );
            }
        )*
    };
}

/// Where each entry point jumps: CEF's function once [`load`] succeeded.
static TABLE: [AtomicPtr<c_void>; Slot::Count as usize] =
    [const { AtomicPtr::new(unloaded as *mut c_void) }; Slot::Count as usize];

/// Whether [`load`] has succeeded in this process.
static LOADED: Mutex<bool> = Mutex::new(false);

/// Open the `libcef.so` at `path` and point every entry point at it. Once
/// per process: the library stays loaded (Chromium cannot be unloaded), and
/// after a success later calls do nothing, whatever their path. A failure
/// (say a system library Chromium needs is missing) can be tried again.
///
/// Call it before any other CEF function. The table is filled before this
/// returns, and the threads that call CEF are started by CEF after it.
pub fn load(path: &Path) -> Result<(), String> {
    let mut loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    if !*loaded {
        open(path)?;
        *loaded = true;
    }
    Ok(())
}

fn open(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{}: not a usable path", path.display()))?;
    // SAFETY: `name` is NUL-terminated and outlives the call. Loading runs
    // the library's initializers; it is CEF's own release, as the component's
    // manifest says.
    let lib = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if lib.is_null() {
        return Err(format!("{} did not load: {}", path.display(), dl_error()));
    }
    let mut found = Vec::with_capacity(NAMES.len());
    let mut missing = Vec::new();
    for name in NAMES {
        // SAFETY: `lib` is open; `name` is NUL-terminated.
        let f = unsafe { libc::dlsym(lib, name.as_ptr().cast()) };
        if f.is_null() {
            missing.push(name.trim_end_matches('\0'));
        }
        found.push(f);
    }
    if !missing.is_empty() {
        // The handle is kept open: closing a library whose initializers ran
        // is not safe for Chromium either.
        return Err(format!(
            "{} is not the CEF release this build is for (it lacks {})",
            path.display(),
            missing.join(", ")
        ));
    }
    for (slot, f) in TABLE.iter().zip(found) {
        slot.store(f, Ordering::Release);
    }
    // GLib, which `libcef.so` links, is loaded with it: its main loop is
    // found now, not loaded again.
    // SAFETY: the name is NUL-terminated; `RTLD_NOLOAD` only looks the
    // library up.
    let glib = unsafe {
        libc::dlopen(
            c"libglib-2.0.so.0".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NOLOAD,
        )
    };
    if !glib.is_null() {
        // SAFETY: `glib` is open; the name is NUL-terminated.
        let f = unsafe { libc::dlsym(glib, c"g_main_context_iteration".as_ptr()) };
        GLIB_ITERATION.store(f, Ordering::Release);
    }
    Ok(())
}

/// GLib's `g_main_context_iteration`, once [`load`] found it.
static GLIB_ITERATION: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Dispatch what is ready on GLib's default main context, without waiting,
/// at most `max` times; call it on the thread that runs CEF's work.
///
/// Chromium's browser process reads its X11 connection from a GLib source
/// on that context. With CEF's external message pump (the embedder calls
/// `cef_do_message_loop_work`), Chromium runs its tasks but never iterates
/// GLib's loop; a GTK app's main loop would. Without this, the events of
/// CEF's and Chromium's windows (configure, expose, input) are never
/// handled.
pub fn dispatch_glib(max: usize) {
    let f = GLIB_ITERATION.load(Ordering::Acquire);
    if f.is_null() {
        return;
    }
    // SAFETY: `f` is `g_main_context_iteration` from the GLib loaded in
    // this process: `gboolean (GMainContext *context, gboolean may_block)`.
    let iteration: unsafe extern "C" fn(*mut c_void, i32) -> i32 =
        unsafe { std::mem::transmute(f) };
    for _ in 0..max {
        // SAFETY: NULL is the default context; 0 never blocks.
        if unsafe { iteration(std::ptr::null_mut(), 0) } == 0 {
            break;
        }
    }
}

/// `dlerror`'s message.
fn dl_error() -> String {
    // SAFETY: `dlerror` returns NULL or a NUL-terminated string valid until
    // the next call on this thread; it is copied at once.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: as above.
        unsafe { std::ffi::CStr::from_ptr(e) }
            .to_string_lossy()
            .into_owned()
    }
}

entry_points! {
    cef_add_cross_origin_whitelist_entry
    cef_api_hash
    cef_api_version
    cef_base64_decode
    cef_base64_encode
    cef_basetime_now
    cef_begin_tracing
    cef_binary_value_create
    cef_browser_host_create_browser
    cef_browser_host_create_browser_sync
    cef_browser_host_get_browser_by_identifier
    cef_browser_view_create
    cef_browser_view_get_for_browser
    cef_clear_cross_origin_whitelist
    cef_clear_scheme_handler_factories
    cef_command_line_create
    cef_command_line_get_global
    cef_component_updater_get
    cef_cookie_manager_get_global_manager
    cef_crash_reporting_enabled
    cef_create_directory
    cef_create_new_temp_directory
    cef_create_temp_directory_in_directory
    cef_create_url
    cef_currently_on
    cef_delete_file
    cef_dictionary_value_create
    cef_directory_exists
    cef_display_convert_screen_point_from_pixels
    cef_display_convert_screen_point_to_pixels
    cef_display_convert_screen_rect_from_pixels
    cef_display_convert_screen_rect_to_pixels
    cef_display_get_alls
    cef_display_get_count
    cef_display_get_matching_bounds
    cef_display_get_nearest_point
    cef_display_get_primary
    cef_do_message_loop_work
    cef_drag_data_create
    cef_end_tracing
    cef_execute_process
    cef_format_url_for_security_display
    cef_get_current_platform_thread_handle
    cef_get_current_platform_thread_id
    cef_get_exit_code
    cef_get_extensions_for_mime_type
    cef_get_mime_type
    cef_get_path
    cef_get_temp_directory
    cef_get_xdisplay
    cef_id_for_command_id_name
    cef_id_for_pack_resource_name
    cef_id_for_pack_string_name
    cef_image_create
    cef_initialize
    cef_is_cert_status_error
    cef_is_rtl
    cef_label_button_create
    cef_launch_process
    cef_list_value_create
    cef_load_crlsets_file
    cef_media_router_get_global
    cef_menu_button_create
    cef_menu_model_create
    cef_now_from_system_trace_time
    cef_panel_create
    cef_parse_json
    cef_parse_json_buffer
    cef_parse_jsonand_return_error
    cef_parse_url
    cef_post_data_create
    cef_post_data_element_create
    cef_post_delayed_task
    cef_post_task
    cef_preference_manager_get_chrome_variations_as_strings
    cef_preference_manager_get_chrome_variations_as_switches
    cef_preference_manager_get_global
    cef_print_settings_create
    cef_process_message_create
    cef_quit_message_loop
    cef_register_scheme_handler_factory
    cef_remove_cross_origin_whitelist_entry
    cef_request_context_cef_create_context_shared
    cef_request_context_create_context
    cef_request_context_get_global_context
    cef_request_create
    cef_resolve_url
    cef_resource_bundle_get_global
    cef_response_create
    cef_run_message_loop
    cef_scroll_view_create
    cef_server_create
    cef_set_crash_key_value
    cef_set_nestable_tasks_allowed
    cef_shared_process_message_builder_create
    cef_shutdown
    cef_stream_reader_create_for_data
    cef_stream_reader_create_for_file
    cef_stream_reader_create_for_handler
    cef_stream_writer_create_for_file
    cef_stream_writer_create_for_handler
    cef_string_ascii_to_utf16
    cef_string_ascii_to_wide
    cef_string_list_alloc
    cef_string_list_append
    cef_string_list_clear
    cef_string_list_copy
    cef_string_list_free
    cef_string_list_size
    cef_string_list_value
    cef_string_map_alloc
    cef_string_map_append
    cef_string_map_clear
    cef_string_map_find
    cef_string_map_free
    cef_string_map_key
    cef_string_map_size
    cef_string_map_value
    cef_string_multimap_alloc
    cef_string_multimap_append
    cef_string_multimap_clear
    cef_string_multimap_enumerate
    cef_string_multimap_find_count
    cef_string_multimap_free
    cef_string_multimap_key
    cef_string_multimap_size
    cef_string_multimap_value
    cef_string_userfree_utf16_alloc
    cef_string_userfree_utf16_free
    cef_string_userfree_utf8_alloc
    cef_string_userfree_utf8_free
    cef_string_userfree_wide_alloc
    cef_string_userfree_wide_free
    cef_string_utf16_clear
    cef_string_utf16_cmp
    cef_string_utf16_set
    cef_string_utf16_to_lower
    cef_string_utf16_to_upper
    cef_string_utf16_to_utf8
    cef_string_utf16_to_wide
    cef_string_utf8_clear
    cef_string_utf8_cmp
    cef_string_utf8_set
    cef_string_utf8_to_utf16
    cef_string_utf8_to_wide
    cef_string_wide_clear
    cef_string_wide_cmp
    cef_string_wide_set
    cef_string_wide_to_utf16
    cef_string_wide_to_utf8
    cef_task_manager_get
    cef_task_runner_get_for_current_thread
    cef_task_runner_get_for_thread
    cef_textfield_create
    cef_thread_create
    cef_time_delta
    cef_time_from_basetime
    cef_time_from_doublet
    cef_time_from_timet
    cef_time_now
    cef_time_to_basetime
    cef_time_to_doublet
    cef_time_to_timet
    cef_uridecode
    cef_uriencode
    cef_urlrequest_create
    cef_v8_backing_store_create
    cef_v8_context_get_current_context
    cef_v8_context_get_entered_context
    cef_v8_context_in_context
    cef_v8_stack_trace_get_current
    cef_v8_value_create_array
    cef_v8_value_create_array_buffer
    cef_v8_value_create_array_buffer_from_backing_store
    cef_v8_value_create_array_buffer_with_copy
    cef_v8_value_create_bool
    cef_v8_value_create_date
    cef_v8_value_create_double
    cef_v8_value_create_function
    cef_v8_value_create_int
    cef_v8_value_create_null
    cef_v8_value_create_object
    cef_v8_value_create_promise
    cef_v8_value_create_string
    cef_v8_value_create_uint
    cef_v8_value_create_undefined
    cef_value_create
    cef_waitable_event_create
    cef_window_create_top_level
    cef_write_json
    cef_xml_reader_create
    cef_zip_directory
    cef_zip_reader_create
}
