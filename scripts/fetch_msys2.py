"""Скачивает из MSYS2 (ucrt64) только то, что нужно для Windows-сборки hyst-cast.

1. Пакеты GStreamer (без зависимостей) + пакеты для компиляции (glib2 и т. п.).
2. Обходит импорты нужных DLL (плагины + их библиотеки); чего не хватает — ищет в индексе
   файлов MSYS2 (ucrt64.files) и докачивает пакет. Повторяет, пока всё не найдено.

python fetch_msys2.py <папка>   → <папка>/ucrt64/{bin,lib,include}
"""
import io
import os
import subprocess
import sys
import tarfile
import urllib.request


def dll_imports(path):
    """Имена DLL, которые импортирует файл: pefile (Windows/CI) или objdump из mingw (Linux)."""
    try:
        import pefile  # noqa: PLC0415
        pe = pefile.PE(path, fast_load=True)
        pe.parse_data_directories(directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"],
                                               pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT"]])
        names = [e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_IMPORT", [])]
        names += [e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_DELAY_IMPORT", [])]
        pe.close()
        return names
    except ImportError:
        out = subprocess.run(["x86_64-w64-mingw32-objdump", "-p", path], capture_output=True, text=True).stdout
        return [l.split("DLL Name:")[1].strip().lower() for l in out.splitlines() if "DLL Name:" in l]
    except Exception:  # noqa: BLE001
        return []


MIRROR = os.environ.get("MSYS2_MIRROR", "https://repo.msys2.org/mingw/ucrt64")
P = "mingw-w64-ucrt-x86_64-"
ROOTS = ["gstreamer", "gst-plugins-base", "gst-plugins-good", "gst-plugins-bad-libs", "gst-plugins-bad",
         "gst-plugins-ugly", "glib2", "gettext-runtime", "libiconv", "pcre2", "libffi", "zlib", "orc"]
PLUGINS = ["coreelements", "app", "videoconvertscale", "videorate", "audioconvert", "audioresample",
           "volume", "autodetect", "videotestsrc", "audiotestsrc", "opus", "videoparsersbad",
           "d3d11", "d3d12", "nvcodec", "qsv", "amfcodec", "mediafoundation", "wasapi2", "wasapi",
           "x264", "openh264", "directsound", "playback", "typefindfunctions", "libav"]
# Тяжёлые/ненужные зависимости, которые точно не нужны выбранным плагинам
SKIP_PKGS = set()


def fetch(url):
    with urllib.request.urlopen(url, timeout=120) as r:
        return r.read()


def read_db(name):
    """Возвращает ({имя пакета: поля}, {папка: поля}) из .db или .files (tar + zstd/gzip)."""
    raw = fetch(f"{MIRROR}/{name}")
    pkgs = {}
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:*") as t:
        for m in t.getmembers():
            if not m.isfile():
                continue
            d = pkgs.setdefault(m.name.split("/")[0], {})
            key = None
            for line in t.extractfile(m).read().decode("utf-8", "replace").splitlines():
                if line.startswith("%") and line.endswith("%"):
                    key = line.strip("%")
                    d.setdefault(key, [])
                elif line and key:
                    d[key].append(line)
    by_name = {v["NAME"][0]: v for v in pkgs.values() if "NAME" in v}
    return by_name, pkgs


def main():
    root = sys.argv[1]
    os.makedirs(os.path.join(root, "pkgs"), exist_ok=True)
    print("индекс пакетов…", flush=True)
    db, _ = read_db("ucrt64.db")
    print("индекс файлов…", flush=True)
    _, files_raw = read_db("ucrt64.files")
    dll_owner = {}
    for dirname, v in files_raw.items():
        name = dirname.rsplit("-", 2)[0]
        for f in v.get("FILES", []):
            if f.startswith("ucrt64/bin/") and f.lower().endswith(".dll"):
                dll_owner.setdefault(os.path.basename(f).lower(), name)

    installed = set()

    def install(name):
        if name in installed or name not in db:
            return
        installed.add(name)
        fn = db[name]["FILENAME"][0]
        path = os.path.join(root, "pkgs", fn)
        if not os.path.exists(path):
            data = fetch(f"{MIRROR}/{fn}")
            open(path, "wb").write(data)
            print(f"  + {name} ({len(data) // 1024} КБ)", flush=True)
        subprocess.run(["bsdtar", "-xf", path, "-C", root, "ucrt64"], check=False, capture_output=True)

    for r in ROOTS:
        install(P + r)

    bin_dir = os.path.join(root, "ucrt64", "bin")
    plug_dir = os.path.join(root, "ucrt64", "lib", "gstreamer-1.0")
    for _ in range(30):
        start = [os.path.join(plug_dir, f"libgst{p}.dll") for p in PLUGINS]
        start = [s for s in start if os.path.exists(s)]
        start += [os.path.join(bin_dir, f) for f in os.listdir(bin_dir) if f.startswith(("libgst", "libglib", "libgobject"))]
        seen, queue, missing = set(), list(start), set()
        while queue:
            cur = queue.pop()
            names = dll_imports(cur)
            for n in names:
                if n in seen:
                    continue
                seen.add(n)
                p = os.path.join(bin_dir, n)
                real = next((os.path.join(bin_dir, f) for f in os.listdir(bin_dir) if f.lower() == n), None)
                if real:
                    queue.append(real)
                elif n in dll_owner:
                    missing.add(dll_owner[n])
                del p
        missing -= installed
        if not missing:
            break
        for m in sorted(missing):
            install(m)
    absent = [p for p in PLUGINS if not os.path.exists(os.path.join(plug_dir, f"libgst{p}.dll"))]
    print(f"готово: {len(installed)} пакетов; плагинов нет в MSYS2: {', '.join(absent) or '—'}")


if __name__ == "__main__":
    main()
