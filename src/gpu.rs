// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Graphics hardware and graphics API discovery.
//!
//! Detection deliberately goes beyond PCI enumeration: an API is only reported
//! as usable when a real device/context can be created.  The OpenGL probe
//! creates an EGL context on the current Wayland display and reads back the
//! renderer string, so llvmpipe/softpipe/swrast are distinguished from a real
//! GPU.  The Vulkan probe creates a Vulkan instance and enumerates physical
//! devices through the loader.  Vulkan rendering itself is not implemented in
//! this version; the probe only feeds diagnostics and future backend choice.

use glow::HasContext as _;
use khronos_egl as egl;
use std::ffi::{CStr, CString, c_char, c_void};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RendererChoice {
    #[default]
    Auto,
    OpenGl,
    Vulkan,
    Software,
}

impl RendererChoice {
    pub const ALL: [RendererChoice; 4] = [
        RendererChoice::Auto,
        RendererChoice::OpenGl,
        RendererChoice::Vulkan,
        RendererChoice::Software,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "opengl" | "gl" | "egl" => Some(Self::OpenGl),
            "vulkan" | "vk" => Some(Self::Vulkan),
            "software" | "cpu" | "shm" => Some(Self::Software),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OpenGl => "opengl",
            Self::Vulkan => "vulkan",
            Self::Software => "software",
        }
    }
}

impl fmt::Display for RendererChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The backend that is actually used for drawing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActiveBackend {
    #[default]
    Software,
    OpenGl,
}

impl ActiveBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::OpenGl => "opengl",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DrmDevice {
    pub card: String,
    pub vendor_id: Option<u32>,
    pub device_id: Option<u32>,
    pub driver: Option<String>,
    pub pci_slot: Option<String>,
}

impl DrmDevice {
    pub fn vendor_name(&self) -> &'static str {
        match self.vendor_id {
            Some(0x1002) => "AMD",
            Some(0x8086) => "Intel",
            Some(0x10de) => "NVIDIA",
            Some(0x1af4) => "Virtio",
            Some(0x1ae0) => "Google",
            Some(0x13b5) => "ARM",
            Some(0x5143) => "Qualcomm",
            _ => "unknown",
        }
    }

    pub fn summary(&self) -> String {
        let mut text = self.card.clone();
        text.push_str(&format!(" [{}, ", self.vendor_name()));
        match (self.vendor_id, self.device_id) {
            (Some(v), Some(d)) => text.push_str(&format!("{v:04x}:{d:04x}]")),
            _ => text.push_str("unknown ids]"),
        }
        if let Some(driver) = &self.driver {
            text.push_str(&format!(" driver={driver}"));
        }
        if let Some(slot) = &self.pci_slot {
            text.push_str(&format!(" pci={slot}"));
        }
        text
    }
}

#[derive(Clone, Debug, Default)]
pub struct EglProbe {
    pub loaded: bool,
    pub ok: bool,
    pub error: Option<String>,
    pub egl_version: Option<String>,
    pub vendor: Option<String>,
    pub client_apis: Option<String>,
    pub api: Option<String>,
    pub renderer: Option<String>,
    pub gl_version: Option<String>,
    pub hardware: bool,
}

impl EglProbe {
    pub fn renderer_string(&self) -> String {
        match (&self.renderer, &self.gl_version) {
            (Some(renderer), Some(version)) => format!("{renderer} ({version})"),
            (Some(renderer), None) => renderer.clone(),
            (None, Some(version)) => version.clone(),
            (None, None) => "unknown".to_string(),
        }
    }

    pub fn is_software_renderer(renderer: &str) -> bool {
        let lower = renderer.to_ascii_lowercase();
        ["llvmpipe", "softpipe", "swrast", "swiftshader", "lavapipe"]
            .iter()
            .any(|needle| lower.contains(needle))
    }
}

#[derive(Clone, Debug, Default)]
pub struct VulkanDevice {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_type: u32,
    pub api_version: String,
    pub driver_version: String,
    pub hardware: bool,
    pub driver: Option<String>,
}

impl VulkanDevice {
    pub fn vendor_name(&self) -> &'static str {
        match self.vendor_id {
            0x1002 => "AMD",
            0x8086 => "Intel",
            0x10de => "NVIDIA",
            0x10005 => "Mesa",
            0x5143 => "Qualcomm",
            0x13b5 => "ARM",
            _ => "unknown",
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self.device_type {
            1 => "integrated",
            2 => "discrete",
            3 => "virtual",
            4 => "cpu",
            _ => "other",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct VulkanProbe {
    pub loader: bool,
    pub instance: bool,
    pub error: Option<String>,
    pub loader_version: Option<String>,
    pub devices: Vec<VulkanDevice>,
    pub icds: Vec<VulkanIcd>,
}

impl VulkanProbe {}

#[derive(Clone, Debug, Default)]
pub struct VulkanIcd {
    pub path: PathBuf,
    pub library: Option<String>,
    pub api_version: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct GpuReport {
    pub drm: Vec<DrmDevice>,
    pub egl: EglProbe,
    pub vulkan: VulkanProbe,
}

#[derive(Clone, Debug)]
pub struct BackendDecision {
    pub requested: RendererChoice,
    pub selected: ActiveBackend,
    pub reason: String,
}

/// Enumerate DRM cards from sysfs. This is used only for the human-readable
/// report; the backend choice is based on real context creation below.
pub fn drm_devices() -> Vec<DrmDevice> {
    let mut cards = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return cards;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let device = entry.path().join("device");
        if !device.exists() {
            continue;
        }
        let vendor_id = read_hex(&device.join("vendor"));
        let device_id = read_hex(&device.join("device"));
        let driver = std::fs::read_link(device.join("driver"))
            .ok()
            .and_then(|link| link.file_name().map(|n| n.to_string_lossy().into_owned()));
        let mut pci_slot = None;
        if let Ok(uevent) = std::fs::read_to_string(device.join("uevent")) {
            for line in uevent.lines() {
                if let Some(value) = line.strip_prefix("PCI_SLOT_NAME=") {
                    pci_slot = Some(value.to_string());
                }
            }
        }
        cards.push(DrmDevice {
            card: name,
            vendor_id,
            device_id,
            driver,
            pci_slot,
        });
    }
    cards.sort_by(|a, b| a.card.cmp(&b.card));
    cards
}

fn read_hex(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

/// Probe EGL/OpenGL by creating a pbuffer context on the given Wayland
/// display. `*mut wl_display` is passed as an opaque pointer.
pub fn probe_egl(display: *mut c_void) -> EglProbe {
    let mut probe = EglProbe::default();
    let egl = match unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() } {
        Ok(egl) => egl,
        Err(error) => {
            probe.error = Some(format!("load libEGL: {error}"));
            return probe;
        }
    };
    probe.loaded = true;
    let result = (|| -> Result<(), String> {
        let display = unsafe { egl.get_display(display) }
            .ok_or_else(|| "eglGetDisplay returned no display".to_string())?;
        egl.initialize(display)
            .map_err(|error| format!("eglInitialize: {error}"))?;
        probe.egl_version = query_string(&egl, Some(display), egl::VERSION);
        probe.vendor = query_string(&egl, Some(display), egl::VENDOR);
        probe.client_apis = query_string(&egl, Some(display), egl::CLIENT_APIS);
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|error| format!("eglBindAPI(OpenGL ES): {error}"))?;
        probe.api = Some("OpenGL ES".to_string());

        let attributes = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES2_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attributes)
            .map_err(|error| format!("eglChooseConfig: {error}"))?
            .ok_or_else(|| "no EGL config with OpenGL ES2".to_string())?;
        let context_attributes = [
            egl::CONTEXT_MAJOR_VERSION,
            2,
            egl::CONTEXT_MINOR_VERSION,
            0,
            egl::NONE,
        ];
        let context = egl
            .create_context(display, config, None, &context_attributes)
            .map_err(|error| format!("eglCreateContext: {error}"))?;
        let surface_attributes = [egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE];
        let surface = egl
            .create_pbuffer_surface(display, config, &surface_attributes)
            .map_err(|error| format!("eglCreatePbufferSurface: {error}"))?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|error| format!("eglMakeCurrent: {error}"))?;

        let glow = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|function| function as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };
        probe.renderer = Some(unsafe { glow.get_parameter_string(glow::RENDERER) });
        probe.gl_version = Some(unsafe { glow.get_parameter_string(glow::VERSION) });
        let vendor = unsafe { glow.get_parameter_string(glow::VENDOR) };
        probe.hardware =
            !EglProbe::is_software_renderer(probe.renderer.as_deref().unwrap_or_default())
                && !EglProbe::is_software_renderer(&vendor);
        probe.ok = true;

        let _ = egl.make_current(display, None, None, None);
        let _ = egl.destroy_surface(display, surface);
        let _ = egl.destroy_context(display, context);
        let _ = egl.terminate(display);
        Ok(())
    })();
    if let Err(error) = result {
        probe.error = Some(error);
    }
    probe
}

fn query_string(
    egl: &egl::DynamicInstance<egl::EGL1_5>,
    display: Option<egl::Display>,
    name: egl::Int,
) -> Option<String> {
    egl.query_string(display, name)
        .ok()
        .map(|value| value.to_string_lossy().into_owned())
}

/// Probe the Vulkan loader: create an instance and enumerate physical devices.
pub fn probe_vulkan() -> VulkanProbe {
    let mut probe = VulkanProbe {
        icds: vulkan_icds(),
        ..Default::default()
    };
    unsafe {
        let library = match libloading::Library::new("libvulkan.so.1") {
            Ok(library) => library,
            Err(error) => {
                probe.error = Some(format!("load libvulkan.so.1: {error}"));
                return probe;
            }
        };
        probe.loader = true;
        let create_instance: libloading::Symbol<VkCreateInstance> =
            match library.get(b"vkCreateInstance\0") {
                Ok(symbol) => symbol,
                Err(error) => {
                    probe.error = Some(format!("vkCreateInstance: {error}"));
                    return probe;
                }
            };
        let enumerate: libloading::Symbol<VkEnumeratePhysicalDevices> =
            match library.get(b"vkEnumeratePhysicalDevices\0") {
                Ok(symbol) => symbol,
                Err(error) => {
                    probe.error = Some(format!("vkEnumeratePhysicalDevices: {error}"));
                    return probe;
                }
            };
        let get_properties: libloading::Symbol<VkGetPhysicalDeviceProperties> =
            match library.get(b"vkGetPhysicalDeviceProperties\0") {
                Ok(symbol) => symbol,
                Err(error) => {
                    probe.error = Some(format!("vkGetPhysicalDeviceProperties: {error}"));
                    return probe;
                }
            };
        let get_properties2 = library
            .get::<VkGetPhysicalDeviceProperties2>(b"vkGetPhysicalDeviceProperties2\0")
            .ok()
            .map(|symbol| *symbol);
        let destroy_instance: libloading::Symbol<VkDestroyInstance> =
            match library.get(b"vkDestroyInstance\0") {
                Ok(symbol) => symbol,
                Err(error) => {
                    probe.error = Some(format!("vkDestroyInstance: {error}"));
                    return probe;
                }
            };
        if let Ok(enumerate_version) =
            library.get::<VkEnumerateInstanceVersion>(b"vkEnumerateInstanceVersion\0")
        {
            let mut version = 0;
            if enumerate_version(&mut version) == 0 {
                probe.loader_version = Some(version_string(version));
            }
        }

        let engine = CString::new("Beyrix").unwrap();
        let application = VkApplicationInfo {
            s_type: 0,
            p_next: std::ptr::null(),
            application_name: engine.as_ptr(),
            application_version: 1,
            engine_name: engine.as_ptr(),
            engine_version: 1,
            api_version: (1 << 22) | (1 << 12), // 1.1
        };
        let create = VkInstanceCreateInfo {
            s_type: 0,
            p_next: std::ptr::null(),
            flags: 0,
            application_info: &application,
            enabled_layer_count: 0,
            enabled_layer_names: std::ptr::null(),
            enabled_extension_count: 0,
            enabled_extension_names: std::ptr::null(),
        };
        let mut instance: VkInstance = std::ptr::null_mut();
        let result = create_instance(&create, std::ptr::null(), &mut instance);
        if result != 0 || instance.is_null() {
            probe.error = Some(format!("vkCreateInstance failed: {result}"));
            return probe;
        }
        probe.instance = true;
        let mut count = 0u32;
        if enumerate(instance, &mut count, std::ptr::null_mut()) != 0 {
            probe.error = Some("vkEnumeratePhysicalDevices failed".to_string());
            destroy_instance(instance, std::ptr::null());
            return probe;
        }
        let mut devices: Vec<VkPhysicalDevice> = vec![std::ptr::null_mut(); count as usize];
        let result = enumerate(instance, &mut count, devices.as_mut_ptr());
        if result != 0 && result != 5 {
            // VK_INCOMPLETE is fine; anything else is not.
            probe.error = Some(format!("vkEnumeratePhysicalDevices failed: {result}"));
            destroy_instance(instance, std::ptr::null());
            return probe;
        }
        for device in devices.into_iter().take(count as usize) {
            if device.is_null() {
                continue;
            }
            let mut buffer = vec![0u8; 4096];
            let mut driver_name = None;
            if let Some(get_properties2) = get_properties2 {
                let mut driver = VkPhysicalDeviceDriverProperties {
                    s_type: 1000196000, // VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES
                    p_next: std::ptr::null_mut(),
                    driver_id: 0,
                    driver_name: [0; 256],
                    driver_info: [0; 256],
                    conformance_version: [0; 4],
                };
                let mut properties = VkPhysicalDeviceProperties2 {
                    s_type: 1000059001, // VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2
                    p_next: (&mut driver as *mut VkPhysicalDeviceDriverProperties).cast(),
                    properties: [0; 4096],
                };
                get_properties2(device, &mut properties);
                buffer.copy_from_slice(&properties.properties);
                if driver.driver_name[0] != 0 {
                    driver_name = Some(
                        CStr::from_ptr(driver.driver_name.as_ptr())
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            } else {
                get_properties(device, buffer.as_mut_ptr().cast());
            }
            let api_version = u32::from_ne_bytes(buffer[0..4].try_into().unwrap());
            let driver_version = u32::from_ne_bytes(buffer[4..8].try_into().unwrap());
            let vendor_id = u32::from_ne_bytes(buffer[8..12].try_into().unwrap());
            let device_id = u32::from_ne_bytes(buffer[12..16].try_into().unwrap());
            let device_type = u32::from_ne_bytes(buffer[16..20].try_into().unwrap());
            let name = CStr::from_ptr(buffer[20..].as_ptr().cast())
                .to_string_lossy()
                .into_owned();
            let hardware = device_type != 4 && !EglProbe::is_software_renderer(&name);
            probe.devices.push(VulkanDevice {
                name,
                vendor_id,
                device_id,
                device_type,
                api_version: version_string(api_version),
                driver_version: version_string(driver_version),
                hardware,
                driver: driver_name,
            });
        }
        destroy_instance(instance, std::ptr::null());
        // Fall back to an ICD path only for loaders that do not expose
        // VK_KHR_get_physical_device_properties2 driver properties.
        for device in &mut probe.devices {
            if device.driver.is_some() {
                continue;
            }
            if let Some(icd) = probe.icds.iter().find(|icd| {
                icd.library
                    .as_deref()
                    .is_some_and(|library| library.contains(vendor_library_hint(device.vendor_id)))
            }) {
                device.driver = icd.library.clone();
            }
        }
    }
    probe
}

fn vendor_library_hint(vendor_id: u32) -> &'static str {
    match vendor_id {
        0x1002 => "radeon",
        0x10005 => "lvp",
        0x8086 => "intel",
        0x10de => "nvidia",
        _ => "",
    }
}

fn version_string(version: u32) -> String {
    format!(
        "{}.{}.{}",
        version >> 22,
        (version >> 12) & 0x3ff,
        version & 0xfff
    )
}

fn vulkan_icds() -> Vec<VulkanIcd> {
    let mut manifests = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for directory in ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"] {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if !seen.insert(canonical) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let library = json_string(&text, "library_path");
                let api_version = json_string(&text, "api_version");
                manifests.push(VulkanIcd {
                    path,
                    library,
                    api_version,
                });
            }
        }
    }
    manifests.sort_by(|a, b| a.path.cmp(&b.path));
    manifests
}

/// Extremely small JSON string field reader, sufficient for ICD manifests.
fn json_string(text: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\"");
    let start = text.find(&pattern)? + pattern.len();
    let rest = &text[start..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Full diagnostic report. `display` may be null when no Wayland display is
/// available; the EGL probe then reports the reason instead of guessing.
pub fn report(display: Option<*mut c_void>) -> GpuReport {
    GpuReport {
        drm: drm_devices(),
        egl: match display {
            Some(display) => probe_egl(display),
            None => EglProbe {
                error: Some("no Wayland display available for an EGL context".to_string()),
                ..Default::default()
            },
        },
        vulkan: probe_vulkan(),
    }
}

/// Choose the drawing backend. Only OpenGL can currently accelerate drawing.
/// Vulkan is never auto-selected: the probe exists, but the rendering backend
/// is not implemented, so claiming acceleration would be dishonest.
pub fn decide(requested: RendererChoice, egl: &EglProbe) -> BackendDecision {
    match requested {
        RendererChoice::Software => BackendDecision {
            requested,
            selected: ActiveBackend::Software,
            reason: "software selected explicitly in configuration".to_string(),
        },
        RendererChoice::Vulkan => BackendDecision {
            requested,
            selected: ActiveBackend::Software,
            reason: "vulkan requested but the Vulkan rendering backend is not implemented; falling back to software".to_string(),
        },
        RendererChoice::OpenGl => {
            if egl.ok {
                BackendDecision {
                    requested,
                    selected: ActiveBackend::OpenGl,
                    reason: format!(
                        "opengl selected explicitly; EGL context created: {}",
                        egl.renderer_string()
                    ),
                }
            } else {
                BackendDecision {
                    requested,
                    selected: ActiveBackend::Software,
                    reason: format!(
                        "opengl requested but EGL/GLES context creation failed ({}); using software",
                        egl.error.as_deref().unwrap_or("unknown error")
                    ),
                }
            }
        }
        RendererChoice::Auto => {
            if !egl.ok {
                BackendDecision {
                    requested,
                    selected: ActiveBackend::Software,
                    reason: format!(
                        "auto: no usable EGL/GLES context ({}); using software",
                        egl.error.as_deref().unwrap_or("unknown error")
                    ),
                }
            } else if egl.hardware {
                BackendDecision {
                    requested,
                    selected: ActiveBackend::OpenGl,
                    reason: format!(
                        "auto: hardware OpenGL renderer detected ({})",
                        egl.renderer_string()
                    ),
                }
            } else {
                BackendDecision {
                    requested,
                    selected: ActiveBackend::Software,
                    reason: format!(
                        "auto: EGL renderer {} is software (llvmpipe-class); software backend is faster",
                        egl.renderer_string()
                    ),
                }
            }
        }
    }
}

pub fn print_report(report: &GpuReport, decision: &BackendDecision) {
    println!("Beyrix graphics report");
    println!();
    println!("DRM devices:");
    if report.drm.is_empty() {
        println!("  none");
    } else {
        for device in &report.drm {
            println!("  {}", device.summary());
        }
    }
    println!();
    println!("OpenGL / EGL:");
    if !report.egl.loaded {
        println!(
            "  unavailable: {}",
            report.egl.error.as_deref().unwrap_or("libEGL not loaded")
        );
    } else if report.egl.ok {
        println!("  context: {}", report.egl.renderer_string());
        println!(
            "  EGL {}; vendor {}; APIs {}",
            report.egl.egl_version.as_deref().unwrap_or("?"),
            report.egl.vendor.as_deref().unwrap_or("?"),
            report.egl.client_apis.as_deref().unwrap_or("?"),
        );
        println!(
            "  hardware accelerated: {}",
            if report.egl.hardware { "yes" } else { "no" }
        );
    } else {
        println!(
            "  unavailable: {}",
            report.egl.error.as_deref().unwrap_or("unknown error")
        );
    }
    println!();
    println!("Vulkan:");
    if !report.vulkan.loader {
        println!(
            "  loader unavailable: {}",
            report
                .vulkan
                .error
                .as_deref()
                .unwrap_or("libvulkan not loaded")
        );
    } else if report.vulkan.devices.is_empty() {
        println!(
            "  loader present, no physical devices{}",
            report
                .vulkan
                .error
                .as_deref()
                .map(|error| format!(" ({error})"))
                .unwrap_or_default()
        );
    } else {
        for device in &report.vulkan.devices {
            println!(
                "  {} [{} {} {}, device {:04x}] driver={} api={} driver-version={}",
                device.name,
                device.vendor_name(),
                device.type_name(),
                if device.hardware {
                    "hardware"
                } else {
                    "software"
                },
                device.device_id,
                device.driver.as_deref().unwrap_or("?"),
                device.api_version,
                device.driver_version,
            );
        }
        println!("  rendering backend: not implemented (diagnostics only)");
    }
    if !report.vulkan.icds.is_empty() {
        println!("  ICD manifests:");
        for icd in &report.vulkan.icds {
            println!(
                "    {} (api {})",
                icd.library.as_deref().unwrap_or("?"),
                icd.api_version.as_deref().unwrap_or("?")
            );
        }
    }
    println!();
    println!("Backend selection:");
    println!("  requested: {}", decision.requested);
    println!("  selected:  {}", decision.selected.name());
    println!("  reason:    {}", decision.reason);
}

// ---- Minimal Vulkan FFI, only what device discovery needs -------------------

type VkInstance = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type VkResult = i32;

#[repr(C)]
struct VkApplicationInfo {
    s_type: u32,
    p_next: *const c_void,
    application_name: *const c_char,
    application_version: u32,
    engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct VkInstanceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    application_info: *const VkApplicationInfo,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
}

type VkCreateInstance = unsafe extern "system" fn(
    *const VkInstanceCreateInfo,
    *const c_void,
    *mut VkInstance,
) -> VkResult;
type VkDestroyInstance = unsafe extern "system" fn(VkInstance, *const c_void);
type VkEnumeratePhysicalDevices =
    unsafe extern "system" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
type VkGetPhysicalDeviceProperties = unsafe extern "system" fn(VkPhysicalDevice, *mut c_void);
type VkGetPhysicalDeviceProperties2 =
    unsafe extern "system" fn(VkPhysicalDevice, *mut VkPhysicalDeviceProperties2);
type VkEnumerateInstanceVersion = unsafe extern "system" fn(*mut u32) -> VkResult;

#[repr(C)]
struct VkPhysicalDeviceProperties2 {
    s_type: u32,
    p_next: *mut c_void,
    properties: [u8; 4096],
}

#[repr(C)]
struct VkPhysicalDeviceDriverProperties {
    s_type: u32,
    p_next: *mut c_void,
    driver_id: u32,
    driver_name: [c_char; 256],
    driver_info: [c_char; 256],
    conformance_version: [u8; 4],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_choice_parses_and_roundtrips() {
        for choice in RendererChoice::ALL {
            assert_eq!(RendererChoice::parse(choice.name()), Some(choice));
        }
        assert_eq!(RendererChoice::parse("GL"), Some(RendererChoice::OpenGl));
        assert_eq!(RendererChoice::parse("cpu"), Some(RendererChoice::Software));
        assert_eq!(RendererChoice::parse("metal"), None);
    }

    #[test]
    fn software_renderers_are_detected() {
        assert!(EglProbe::is_software_renderer(
            "llvmpipe (LLVM 22.1.8, 256 bits)"
        ));
        assert!(EglProbe::is_software_renderer("Mesa swrast"));
        assert!(EglProbe::is_software_renderer("lavapipe"));
        assert!(!EglProbe::is_software_renderer(
            "AMD Radeon Graphics (radeonsi, renoir, ACO)"
        ));
        assert!(!EglProbe::is_software_renderer("NVIDIA GeForce RTX 3060"));
    }

    #[test]
    fn decision_never_claims_vulkan_rendering() {
        let egl = EglProbe {
            ok: true,
            hardware: true,
            renderer: Some("AMD".into()),
            ..Default::default()
        };
        let decision = decide(RendererChoice::Vulkan, &egl);
        assert_eq!(decision.selected, ActiveBackend::Software);
        assert!(decision.reason.contains("not implemented"));
    }

    #[test]
    fn auto_rejects_llvmpipe_and_accepts_hardware() {
        let software = EglProbe {
            ok: true,
            hardware: false,
            renderer: Some("llvmpipe".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(RendererChoice::Auto, &software).selected,
            ActiveBackend::Software
        );
        let hardware = EglProbe {
            ok: true,
            hardware: true,
            renderer: Some("AMD Radeon".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(RendererChoice::Auto, &hardware).selected,
            ActiveBackend::OpenGl
        );
    }

    #[test]
    fn explicit_opengl_falls_back_when_context_is_unavailable() {
        let failed = EglProbe {
            loaded: true,
            ok: false,
            error: Some("no config".into()),
            ..Default::default()
        };
        let decision = decide(RendererChoice::OpenGl, &failed);
        assert_eq!(decision.selected, ActiveBackend::Software);
        assert!(decision.reason.contains("no config"));
    }

    #[test]
    fn json_reader_reads_icd_fields() {
        let text = r#"{"file_format_version":"1.0.0","ICD":{"library_path":"/usr/lib64/libvulkan_radeon.so","api_version":"1.4.354"}}"#;
        assert_eq!(
            json_string(text, "library_path").as_deref(),
            Some("/usr/lib64/libvulkan_radeon.so")
        );
        assert_eq!(json_string(text, "api_version").as_deref(), Some("1.4.354"));
        assert_eq!(json_string(text, "missing"), None);
    }
}
