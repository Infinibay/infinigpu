# Copyright 2024 Infinibay
# SPDX-License-Identifier: MIT
#
# Mirrors src/virtio/vulkan/meson.build (venus), adapted to the infinigpu prefix.

infinigpu_entrypoints = custom_target(
  'infinigpu_entrypoints',
  input : [vk_entrypoints_gen, vk_api_xml],
  output : ['infinigpu_entrypoints.h', 'infinigpu_entrypoints.c'],
  command : [
    prog_python, '@INPUT0@', '--xml', '@INPUT1@', '--proto', '--weak',
    '--out-h', '@OUTPUT0@', '--out-c', '@OUTPUT1@', '--prefix', 'infinigpu',
    '--beta', with_vulkan_beta.to_string()
  ],
)

infinigpu_icd = custom_target(
  'infinigpu_icd',
  input : [vk_icd_gen, vk_api_xml],
  output : 'infinigpu_icd.@0@.json'.format(host_machine.cpu()),
  command : [
    prog_python, '@INPUT0@',
    '--api-version', '1.3', '--xml', '@INPUT1@',
    '--lib-path', join_paths(get_option('prefix'), get_option('libdir'),
                             'libvulkan_infinigpu.so'),
    '--out', '@OUTPUT@',
  ],
  build_by_default : true,
  install_dir : with_vulkan_icd_dir,
  install_tag : 'runtime',
  install : true,
)

_dev_icdname = 'infinigpu_devenv_icd.@0@.json'.format(host_machine.cpu())
_dev_icd = custom_target(
  'infinigpu_devenv_icd',
  input : [vk_icd_gen, vk_api_xml],
  output : _dev_icdname,
  command : [
    prog_python, '@INPUT0@',
    '--api-version', '1.3', '--xml', '@INPUT1@',
    '--lib-path', meson.current_build_dir() / 'libvulkan_infinigpu.so',
    '--out', '@OUTPUT@',
  ],
  build_by_default : true,
)

devenv.append('VK_DRIVER_FILES', _dev_icd.full_path())
# Deprecated: replaced by VK_DRIVER_FILES above
devenv.append('VK_ICD_FILENAMES', _dev_icd.full_path())

libinfinigpu_files = files(
  'infinigpu_cmd_buffer.c',
  'infinigpu_descriptor.c',
  'infinigpu_device.c',
  'infinigpu_forwarded.c',
  'infinigpu_icd.c',
  'infinigpu_image.c',
  'infinigpu_instance.c',
  'infinigpu_kmd.c',
  'infinigpu_memory.c',
  'infinigpu_physical_device.c',
  'infinigpu_pipeline.c',
  'infinigpu_sync.c',
  'infinigpu_wsi.c',
)

infinigpu_deps = [
  dep_libdrm,
  dep_thread,
  idep_mesautil,
  idep_vulkan_util,
  idep_vulkan_lite_runtime,
  # Common WSI (VK_KHR_surface/swapchain + headless/display present). link_whole
  # inside the idep, so our weak entrypoint overloads resolve. Pulls only libdrm
  # here (-Dplatforms= empty → no xcb/wayland); libudev is optional (hotplug).
  idep_vulkan_wsi,
]

infinigpu_flags = [
  no_override_init_args,
]

libvulkan_infinigpu = shared_library(
  'vulkan_infinigpu',
  [libinfinigpu_files, infinigpu_entrypoints],
  include_directories : [
    inc_include, inc_src,
  ],
  dependencies : infinigpu_deps,
  c_args : infinigpu_flags,
  link_args : [vulkan_icd_link_args, ld_args_bsymbolic, ld_args_gc_sections],
  link_depends : vulkan_icd_link_depends,
  gnu_symbol_visibility : 'hidden',
  install : true,
)
