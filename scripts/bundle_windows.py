"""Собирает папку hyst-cast для Windows: exe + только нужные DLL GStreamer + плагины.

Нужные DLL находятся обходом таблиц импорта (pefile), начиная с exe и выбранных плагинов.
Системные DLL Windows не копируются, кроме рантайма MSVC (его может не быть у друга).

python bundle_windows.py <hyst-cast.exe> <корень GStreamer> <выходная папка>
"""
import os
import shutil
import sys

import pefile

# Плагины, которые реально использует приложение
PLUGINS = [
    "coreelements", "app", "videoconvertscale", "videorate", "audioconvert", "audioresample",
    "volume", "autodetect", "videotestsrc", "audiotestsrc", "opus", "videoparsersbad",
    "d3d11", "d3d12", "nvcodec", "qsv", "amfcodec", "mediafoundation", "wasapi2", "wasapi",
    "x264", "openh264", "directsound",
]
MSVC_RUNTIME = ("vcruntime140", "vcruntime140_1", "msvcp140", "msvcp140_1", "msvcp140_2", "concrt140")


def imports(path):
    try:
        pe = pefile.PE(path, fast_load=True)
        pe.parse_data_directories(directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"],
                                               pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT"]])
        names = [e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_IMPORT", [])]
        names += [e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_DELAY_IMPORT", [])]
        pe.close()
        return names
    except Exception as e:  # noqa: BLE001
        print(f"  ! не разобрать {path}: {e}")
        return []


def main():
    exe, gst_root, out = sys.argv[1:4]
    gst_bin = os.path.join(gst_root, "bin")
    gst_plugins = os.path.join(gst_root, "lib", "gstreamer-1.0")
    sys32 = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32")
    gst_dlls = {f.lower(): os.path.join(gst_bin, f) for f in os.listdir(gst_bin) if f.lower().endswith(".dll")}

    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(os.path.join(out, "gstreamer-plugins"))
    shutil.copy2(exe, out)

    queue = [os.path.join(out, os.path.basename(exe))]
    for p in PLUGINS:
        src = os.path.join(gst_plugins, f"gst{p}.dll")
        if os.path.exists(src):
            dst = os.path.join(out, "gstreamer-plugins", os.path.basename(src))
            shutil.copy2(src, dst)
            queue.append(dst)
        else:
            print(f"  - плагина {p} нет в этой сборке GStreamer")

    seen = set()
    while queue:
        cur = queue.pop()
        for dll in imports(cur):
            if dll in seen:
                continue
            seen.add(dll)
            if dll in gst_dlls:
                dst = os.path.join(out, os.path.basename(gst_dlls[dll]))
                shutil.copy2(gst_dlls[dll], dst)
                queue.append(dst)
            elif dll.rsplit(".", 1)[0] in MSVC_RUNTIME and os.path.exists(os.path.join(sys32, dll)):
                shutil.copy2(os.path.join(sys32, dll), os.path.join(out, dll))

    total = 0
    for root, _, files in os.walk(out):
        total += sum(os.path.getsize(os.path.join(root, f)) for f in files)
    n = sum(len(f) for _, _, f in os.walk(out))
    print(f"готово: {n} файлов, {total / 1048576:.1f} МБ")


if __name__ == "__main__":
    main()
