// SPDX-License-Identifier: GPL-3.0-only
use anyhow::Result;
use winit::window::Window;

use crate::{GlassEffect, GlassMaterial, MaterialParameters};

#[derive(Clone, Copy, Debug, Default)]
pub struct AccessibilityPreferences {
    pub reduce_motion: bool,
    pub reduce_transparency: bool,
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use anyhow::{ensure, Context};
    use objc2::declare::ClassBuilder;
    use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
    use objc2::{class, msg_send, sel};
    use objc2_foundation::{NSPoint, NSRect, NSString};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::ptr;
    use std::sync::OnceLock;

    const GLASS_VIEW_TAG: isize = 0x475354;
    const ACRYLIC_BACKDROP_TAG: isize = 0x474142;
    const HUD_MATERIAL: isize = 13;

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    extern "C" fn glass_tag(_: &AnyObject, _: Sel) -> isize {
        GLASS_VIEW_TAG
    }

    extern "C" fn acrylic_backdrop_tag(_: &AnyObject, _: Sel) -> isize {
        ACRYLIC_BACKDROP_TAG
    }

    extern "C" fn ignore_pointer(_: &AnyObject, _: Sel, _: NSPoint) -> *mut AnyObject {
        ptr::null_mut()
    }

    fn glass_class() -> &'static AnyClass {
        static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
        CLASS.get_or_init(|| {
            let mut builder =
                ClassBuilder::new("PlyraGlassTerminalMaterialView", class!(NSVisualEffectView))
                    .expect("unique native material class");
            // The material is display-only and cannot take mouse input from the PTY.
            unsafe {
                builder.add_method(sel!(tag), glass_tag as extern "C" fn(_, _) -> _);
                builder.add_method(
                    sel!(hitTest:),
                    ignore_pointer as extern "C" fn(_, _, _) -> _,
                );
            }
            builder.register()
        })
    }

    fn liquid_glass_class() -> Option<&'static AnyClass> {
        static CLASS: OnceLock<Option<&'static AnyClass>> = OnceLock::new();
        *CLASS.get_or_init(|| {
            let superclass = AnyClass::get("NSGlassEffectView")?;
            let mut builder = ClassBuilder::new("PlyraLiquidGlassMaterialView", superclass)
                .expect("unique native liquid glass class");
            unsafe {
                builder.add_method(sel!(tag), glass_tag as extern "C" fn(_, _) -> _);
                builder.add_method(
                    sel!(hitTest:),
                    ignore_pointer as extern "C" fn(_, _, _) -> _,
                );
            }
            Some(builder.register())
        })
    }

    fn acrylic_backdrop_class() -> &'static AnyClass {
        static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
        CLASS.get_or_init(|| {
            let mut builder = ClassBuilder::new("PlyraAcrylicBackdropView", glass_class())
                .expect("unique native backdrop class");
            unsafe {
                builder.add_method(sel!(tag), acrylic_backdrop_tag as extern "C" fn(_, _) -> _);
            }
            builder.register()
        })
    }

    unsafe fn configure_visual_effect(
        view: &AnyObject,
        corner_radius: f64,
        material: isize,
        state: isize,
    ) {
        unsafe {
            let _: () = msg_send![view, setWantsLayer: Bool::YES];
            let layer: *mut AnyObject = msg_send![view, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: corner_radius];
                let _: () = msg_send![layer, setMasksToBounds: Bool::new(corner_radius > 0.0)];
            }
            let _: () = msg_send![view, setMaterial: material];
            let _: () = msg_send![view, setBlendingMode: 0_isize]; // BehindWindow
            let _: () = msg_send![view, setState: state];
            let appearance_name = NSString::from_str("NSAppearanceNameVibrantDark");
            let appearance: *mut AnyObject =
                msg_send![class!(NSAppearance), appearanceNamed: &*appearance_name];
            if !appearance.is_null() {
                let _: () = msg_send![view, setAppearance: appearance];
            }
        }
    }

    pub fn preferences() -> AccessibilityPreferences {
        unsafe {
            let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            let supports_motion: Bool = msg_send![workspace, respondsToSelector: sel!(accessibilityDisplayShouldReduceMotion)];
            let supports_transparency: Bool = msg_send![workspace, respondsToSelector: sel!(accessibilityDisplayShouldReduceTransparency)];
            let motion: Bool = if supports_motion.as_bool() {
                msg_send![workspace, accessibilityDisplayShouldReduceMotion]
            } else {
                Bool::NO
            };
            let transparency: Bool = if supports_transparency.as_bool() {
                msg_send![workspace, accessibilityDisplayShouldReduceTransparency]
            } else {
                Bool::NO
            };
            AccessibilityPreferences {
                reduce_motion: motion.as_bool(),
                reduce_transparency: transparency.as_bool(),
            }
        }
    }

    pub fn configure(
        window: &Window,
        effect: GlassEffect,
        material: GlassMaterial,
        corner_radius: f32,
        acrylic_blur: bool,
    ) -> Result<bool> {
        let is_main: Bool = unsafe { msg_send![class!(NSThread), isMainThread] };
        ensure!(
            is_main.as_bool(),
            "Native glass must be configured on the AppKit thread"
        );
        let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
            return Ok(false);
        };
        let enabled = effect != GlassEffect::Off && !preferences().reduce_transparency;
        let corner_radius = f64::from(corner_radius.clamp(0.0, 48.0));
        let rounded = corner_radius > 0.0;
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<AnyObject>();
            let legacy_child: *mut AnyObject = msg_send![view, viewWithTag: GLASS_VIEW_TAG];
            if !legacy_child.is_null() {
                let _: () = msg_send![legacy_child, removeFromSuperview];
            }
            let native_window: *mut AnyObject = msg_send![view, window];
            ensure!(
                !native_window.is_null(),
                "The native terminal view has no window"
            );
            // Clip the existing Metal-backed content view as well as its native
            // material sibling. A zero radius explicitly removes a prior mask.
            let _: () = msg_send![view, setWantsLayer: Bool::YES];
            let backing_layer: *mut AnyObject = msg_send![view, layer];
            if !backing_layer.is_null() {
                let _: () = msg_send![backing_layer, setCornerRadius: corner_radius];
                let _: () = msg_send![backing_layer, setMasksToBounds: Bool::new(rounded)];
            }
            let _: () = msg_send![native_window, invalidateShadow];
            let container: *mut AnyObject = msg_send![view, superview];
            if container.is_null() {
                // A not-yet-attached native view can appear during window setup.
                // Keep a usable opaque surface until a presentation can retry.
                let _: () = msg_send![native_window, setOpaque: Bool::new(!rounded)];
                return Ok(false);
            }
            let mut old: *mut AnyObject = msg_send![container, viewWithTag: GLASS_VIEW_TAG];
            let backdrop: *mut AnyObject = msg_send![container, viewWithTag: ACRYLIC_BACKDROP_TAG];
            let _: () = msg_send![native_window, setOpaque: Bool::new(!enabled && !rounded)];
            if !backing_layer.is_null() {
                let is_metal: Bool = msg_send![backing_layer, isKindOfClass: class!(CAMetalLayer)];
                if !is_metal.as_bool() {
                    // The surface configuration owns the Metal child opacity.
                    // Restore only the view's existing backing layer here.
                    let _: () =
                        msg_send![backing_layer, setOpaque: Bool::new(!enabled && !rounded)];
                }
            }
            let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![native_window, setBackgroundColor: clear];
            if !enabled {
                if !old.is_null() {
                    let _: () = msg_send![old, removeFromSuperview];
                }
                if !backdrop.is_null() {
                    let _: () = msg_send![backdrop, removeFromSuperview];
                }
                return Ok(false);
            }
            let frame: NSRect = msg_send![view, frame];
            let liquid_class = match material {
                GlassMaterial::LiquidClear | GlassMaterial::LiquidRegular => liquid_glass_class(),
                _ => None,
            };
            let selected_class = liquid_class.unwrap_or_else(glass_class);
            if !old.is_null() {
                let matches_class: Bool = msg_send![old, isKindOfClass: selected_class];
                if !matches_class.as_bool() {
                    let _: () = msg_send![old, removeFromSuperview];
                    old = ptr::null_mut();
                }
            }
            let created = old.is_null();
            let effect_view: *mut AnyObject = if created {
                let allocated: *mut AnyObject = msg_send![selected_class, alloc];
                msg_send![allocated, initWithFrame: frame]
            } else {
                old
            };
            let effect_view = effect_view
                .as_ref()
                .context("Cannot create the native glass material")?;
            let _: () = msg_send![effect_view, setFrame: frame];
            if liquid_class.is_some() {
                let style: isize = if material == GlassMaterial::LiquidClear {
                    1
                } else {
                    0
                };
                let _: () = msg_send![effect_view, setStyle: style];
                let _: () = msg_send![effect_view, setCornerRadius: corner_radius];
                let no_tint: *mut AnyObject = ptr::null_mut();
                let _: () = msg_send![effect_view, setTintColor: no_tint];
                // This property arrived in macOS 27. Keep the display-only
                // surface static even when that newer selector is available.
                let interactive: Bool =
                    msg_send![effect_view, respondsToSelector: sel!(setEffectIsInteractive:)];
                if interactive.as_bool() {
                    let _: () = msg_send![effect_view, setEffectIsInteractive: Bool::NO];
                }
                let content: *mut AnyObject = msg_send![effect_view, contentView];
                if content.is_null() {
                    let bounds: NSRect = msg_send![effect_view, bounds];
                    let allocated: *mut AnyObject = msg_send![class!(NSView), alloc];
                    let content: *mut AnyObject = msg_send![allocated, initWithFrame: bounds];
                    let _: () = msg_send![content, setAutoresizingMask: 18_usize];
                    // Use the documented contentView relationship. Winit and
                    // its CAMetalLayer retain their existing sibling position.
                    let _: () = msg_send![effect_view, setContentView: content];
                    let _: () = msg_send![content, release];
                }
            } else {
                let material_value: isize = match material {
                    GlassMaterial::Sidebar => 7,
                    GlassMaterial::UnderWindowBackground => 21,
                    // Older AppKit versions use the existing public HUD material.
                    _ => HUD_MATERIAL,
                };
                configure_visual_effect(effect_view, corner_radius, material_value, 0);
            }
            let _: () = msg_send![effect_view, setAutoresizingMask: 18_usize];
            // A child view composites above its parent's CAMetalLayer. Use a
            // sibling below the content view so AppKit layout cannot cover Metal.
            if created {
                let _: () = msg_send![container,
                    addSubview: effect_view
                    positioned: -1_isize
                    relativeTo: view
                ];
                let _: () = msg_send![effect_view, release];
            }
            if liquid_class.is_some() && acrylic_blur {
                let new_backdrop = backdrop.is_null();
                let backdrop = if new_backdrop {
                    let allocated: *mut AnyObject = msg_send![acrylic_backdrop_class(), alloc];
                    let created: *mut AnyObject = msg_send![allocated, initWithFrame: frame];
                    created
                } else {
                    backdrop
                };
                let backdrop = backdrop
                    .as_ref()
                    .context("Cannot create the acrylic backdrop")?;
                let _: () = msg_send![backdrop, setFrame: frame];
                // Keep the backdrop active when the terminal loses focus. AppKit
                // composites the desktop here without another Metal render pass.
                configure_visual_effect(backdrop, corner_radius, HUD_MATERIAL, 1);
                let _: () = msg_send![backdrop, setAutoresizingMask: 18_usize];
                if new_backdrop {
                    let _: () = msg_send![container,
                        addSubview: backdrop
                        positioned: -1_isize
                        relativeTo: effect_view
                    ];
                    let _: () = msg_send![backdrop, release];
                }
            } else if !backdrop.is_null() {
                let _: () = msg_send![backdrop, removeFromSuperview];
            }
            let _: () = msg_send![view, setNeedsDisplay: Bool::YES];
        }
        Ok(true)
    }

    pub fn diagnostics(window: &Window) -> Result<String> {
        let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
            return Ok("native_handle=unavailable".to_owned());
        };
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<AnyObject>();
            let parent: *mut AnyObject = msg_send![view, superview];
            let layer: *mut AnyObject = msg_send![view, layer];
            let native_window: *mut AnyObject = msg_send![view, window];
            let window_opaque: Bool = msg_send![native_window, isOpaque];
            let layer_opaque: Bool = if layer.is_null() {
                Bool::YES
            } else {
                msg_send![layer, isOpaque]
            };
            let mut metal_layer: *mut AnyObject = ptr::null_mut();
            if !layer.is_null() {
                let is_metal: Bool = msg_send![layer, isKindOfClass: class!(CAMetalLayer)];
                if is_metal.as_bool() {
                    metal_layer = layer;
                } else {
                    // wgpu 24 adds its CAMetalLayer directly below the backing layer.
                    let sublayers: *mut AnyObject = msg_send![layer, sublayers];
                    let count: usize = msg_send![sublayers, count];
                    for index in 0..count.min(16) {
                        let child: *mut AnyObject = msg_send![sublayers, objectAtIndex: index];
                        let is_metal: Bool = msg_send![child, isKindOfClass: class!(CAMetalLayer)];
                        if is_metal.as_bool() {
                            metal_layer = child;
                            break;
                        }
                    }
                }
            }
            let metal_layer_opaque = if metal_layer.is_null() {
                None
            } else {
                let opaque: Bool = msg_send![metal_layer, isOpaque];
                Some(opaque.as_bool())
            };
            let material: *mut AnyObject = if parent.is_null() {
                ptr::null_mut()
            } else {
                msg_send![parent, viewWithTag: GLASS_VIEW_TAG]
            };
            let backdrop: *mut AnyObject = if parent.is_null() {
                ptr::null_mut()
            } else {
                msg_send![parent, viewWithTag: ACRYLIC_BACKDROP_TAG]
            };
            let material_parent: *mut AnyObject = if material.is_null() {
                ptr::null_mut()
            } else {
                msg_send![material, superview]
            };
            let actual_material = if material.is_null() {
                "none".to_owned()
            } else {
                let liquid = AnyClass::get("NSGlassEffectView").is_some_and(|glass| {
                    let is_liquid: Bool = msg_send![material, isKindOfClass: glass];
                    is_liquid.as_bool()
                });
                if liquid {
                    let style: isize = msg_send![material, style];
                    format!(
                        "NSGlassEffectView.{}",
                        if style == 1 { "Clear" } else { "Regular" }
                    )
                } else {
                    let value: isize = msg_send![material, material];
                    format!("NSVisualEffectView.material{value}")
                }
            };
            let frame: NSRect = msg_send![view, frame];
            let bounds: NSRect = msg_send![view, bounds];
            let corner_radius: f64 = if layer.is_null() {
                0.0
            } else {
                msg_send![layer, cornerRadius]
            };
            let masks_to_bounds: Bool = if layer.is_null() {
                Bool::NO
            } else {
                msg_send![layer, masksToBounds]
            };
            let class_name = |object: *mut AnyObject| {
                object
                    .as_ref()
                    .map(|object| object.class().name().to_owned())
                    .unwrap_or_else(|| "none".to_owned())
            };
            Ok(format!("view={} parent={} layer={} metal_layer={} metal_layer_opaque={metal_layer_opaque:?} material={} actual_material={actual_material} acrylic_backdrop={} material_parent={} material_is_sibling={} window_opaque={} layer_opaque={} corner_radius={corner_radius} masks_to_bounds={} frame={frame:?} bounds={bounds:?}",
                class_name(view), class_name(parent), class_name(layer), class_name(metal_layer), class_name(material), !backdrop.is_null(),
                class_name(material_parent), !material.is_null() && material_parent == parent,
                window_opaque.as_bool(), layer_opaque.as_bool(), masks_to_bounds.as_bool()))
        }
    }
}

pub fn accessibility_preferences() -> AccessibilityPreferences {
    #[cfg(target_os = "macos")]
    return macos::preferences();
    #[cfg(windows)]
    return crate::windows_glass::preferences();
    #[cfg(not(any(target_os = "macos", windows)))]
    AccessibilityPreferences::default()
}

pub fn configure_native_glass(window: &Window, effect: GlassEffect) -> Result<bool> {
    configure_with_material(window, effect, GlassMaterial::default())
}

pub(crate) fn configure_with_material(
    window: &Window,
    effect: GlassEffect,
    material: GlassMaterial,
) -> Result<bool> {
    configure_with_geometry(window, effect, material, 0.0, MaterialParameters::default())
}

pub(crate) fn configure_with_geometry(
    window: &Window,
    effect: GlassEffect,
    material: GlassMaterial,
    corner_radius: f32,
    parameters: MaterialParameters,
) -> Result<bool> {
    let _ = parameters;
    let _ = (material, corner_radius);
    #[cfg(target_os = "macos")]
    return macos::configure(
        window,
        effect,
        material,
        corner_radius,
        parameters.backdrop_blur_radius_px > 0.0,
    );
    #[cfg(target_os = "linux")]
    return crate::linux_glass::configure(
        window,
        effect,
        corner_radius,
        parameters.backdrop_blur_radius_px,
    );
    #[cfg(windows)]
    return crate::windows_glass::configure(
        window,
        effect,
        material,
        corner_radius,
        parameters.backdrop_blur_radius_px,
    );
    #[cfg(target_os = "android")]
    return crate::android_glass::configure(effect, parameters.backdrop_blur_radius_px);
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    )))]
    {
        let _ = (window, effect, material, corner_radius);
        Ok(false)
    }
}

pub(crate) fn diagnostics(window: &Window) -> Result<String> {
    #[cfg(target_os = "macos")]
    return macos::diagnostics(window);
    #[cfg(target_os = "linux")]
    return crate::linux_glass::diagnostics(window);
    #[cfg(windows)]
    return crate::windows_glass::diagnostics(window);
    #[cfg(target_os = "android")]
    {
        let _ = window;
        return Ok(crate::android_glass::diagnostics());
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    )))]
    {
        let _ = window;
        Ok("native_glass=unavailable".to_owned())
    }
}

pub(crate) fn poll(window: &Window) {
    #[cfg(target_os = "linux")]
    crate::linux_glass::poll(window);
    let _ = window;
}

pub(crate) fn release(window: &Window) {
    #[cfg(target_os = "linux")]
    crate::linux_glass::release(window);
    let _ = window;
}

// FVPROV1 21628 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTdmYmNiYmVmNGM4YWM4MzRkYTUyYjk4ZTg0YTgwYjRkNjdkY2U1NDUwOWY0Y2Y2OGUwNDk1ZTNjZWYyM2I1NCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4yIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjIiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOWNmMmIyOWIzZDMyYzgxZmM4ODcxYWRjNTNlMzcwMTg2NjNjYjUyZGM3NjBhZjlmZjMxN2M3Zjc3MTVmMjM0N2E4ZDQ2MmU1M2NhMmYzYTE3YzdkZWU4Njc2MmJmOTgxZDYyOTg4ZGU4MzAxYzc4MWY5MzhiZDc4YWQyNGM3MDgifV19XQ== FVPROV1-END
