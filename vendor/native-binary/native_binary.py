"""Read platform executable metadata without running untrusted binary content.

Windows import-table validation follows the Constellation computer-helper guard;
normal and delayed imports both matter. OS DLLs remain valid dependencies.
"""
import hashlib
import struct

def inspect_pe(data, architecture):
    """Read ordinary and delay-load import tables without executing the PE file."""
    def unpack(fmt, offset):
        if offset < 0 or offset + struct.calcsize(fmt) > len(data):
            raise ValueError("Truncated PE metadata")
        return struct.unpack_from(fmt, data, offset)

    if len(data) > 80 * 1024 * 1024 or data[:2] != b"MZ":
        raise ValueError("Expected a bounded Windows executable")
    pe = unpack("<I", 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Missing PE signature")
    machine, count = unpack("<HH", pe + 4)
    optional_size = unpack("<H", pe + 20)[0]
    optional = pe + 24
    if machine != {"x86_64": 0x8664, "aarch64": 0xAA64}[architecture] or unpack("<H", optional)[0] != 0x20B:
        raise ValueError("Wrong PE32+ executable architecture")
    if not 0 < count <= 96 or optional_size < 112:
        raise ValueError("Invalid PE header sizes")
    image_base = unpack("<Q", optional + 24)[0]
    header_size = unpack("<I", optional + 60)[0]
    directory_count = unpack("<I", optional + 108)[0]
    if directory_count > (optional_size - 112) // 8:
        raise ValueError("Truncated PE directory table")
    sections = []
    for index in range(count):
        start = optional + optional_size + index * 40
        virtual_size, address, raw_size, raw = unpack("<IIII", start + 8)
        if raw + raw_size > len(data):
            raise ValueError("Truncated PE section")
        sections.append((address, virtual_size, raw, raw_size))

    def offset(rva, size):
        if rva < header_size and rva + size <= min(header_size, len(data)):
            return rva
        for address, _virtual_size, raw, raw_size in sections:
            delta = rva - address
            if 0 <= delta and delta + size <= raw_size:
                return raw + delta
        raise ValueError("Import metadata points outside stored PE sections")

    def dll_name(rva):
        result = bytearray()
        for index in range(256):
            value = data[offset(rva + index, 1)]
            if value == 0:
                if not result:
                    raise ValueError("Empty DLL import")
                return result.decode("ascii")
            result.append(value)
        raise ValueError("Unterminated DLL import")

    def imports(directory, width, delayed=False):
        if directory >= directory_count:
            return []
        address, size = unpack("<II", optional + 112 + directory * 8)
        if address == 0 and size == 0:
            return []
        if address == 0 or size < width or size > 1024 * width:
            raise ValueError("Invalid import directory size")
        names = []
        for index in range(size // width):
            values = unpack("<" + "I" * (width // 4), offset(address + index * width, width))
            if not any(values):
                return names
            if delayed:
                flags, name = values[:2]
                if flags not in (0, 1):
                    raise ValueError("Unknown delay-import address mode")
                name = name if flags == 1 else name - image_base
            else:
                name = values[3]
            names.append(dll_name(name))
        raise ValueError("Unterminated import directory")

    ordinary = imports(1, 20)
    delayed = imports(13, 32, delayed=True)
    external = sorted({name for name in ordinary + delayed
                       if name.lower().startswith(("vcruntime", "msvcp"))})
    return {"binary_sha256": hashlib.sha256(data).hexdigest(),
            "architecture": architecture, "imports": ordinary, "delay_imports": delayed,
            "external_vc_runtime": external, "standalone_vc_runtime": not external}



def inspect(data, platform):
    family, architecture = platform.split("-", 1)
    if architecture not in ("x86_64", "aarch64"):
        raise ValueError("Unsupported architecture")
    if family == "windows":
        result = inspect_pe(data, architecture)
        if not result["standalone_vc_runtime"]:
            raise ValueError("Executable requires an external Visual C++ runtime: " + ", ".join(result["external_vc_runtime"]))
        return result
    if family == "linux":
        if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
            raise ValueError("Expected a little-endian ELF64 executable")
        kind, machine = struct.unpack_from("<HH", data, 16)
        if kind not in (2, 3) or machine != {"x86_64": 62, "aarch64": 183}[architecture]:
            raise ValueError("Wrong ELF executable architecture")
        return {"format": "ELF64", "architecture": architecture,
                "binary_sha256": hashlib.sha256(data).hexdigest()}
    if family == "macos":
        if len(data) < 32 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("Expected a native Mach-O64 executable")
        cpu, subtype, kind, count, command_bytes = struct.unpack_from("<IIIII", data, 4)
        if kind != 2 or cpu != {"x86_64": 0x01000007, "aarch64": 0x0100000c}[architecture]:
            raise ValueError("Wrong Mach-O executable architecture")
        if count > 4096 or 32 + command_bytes > len(data):
            raise ValueError("Truncated Mach-O commands")
        position = 32
        minimum = None
        for _ in range(count):
            if position + 8 > 32 + command_bytes:
                raise ValueError("Truncated Mach-O load command")
            command, size = struct.unpack_from("<II", data, position)
            if size < 8 or position + size > 32 + command_bytes:
                raise ValueError("Invalid Mach-O load command")
            if command == 0x32:
                if size < 24:
                    raise ValueError("Invalid Mach-O build version")
                os_id, version = struct.unpack_from("<II", data, position + 8)
                if os_id != 1:
                    raise ValueError("Executable is not for macOS")
                minimum = version
            elif command == 0x24:
                if size < 16:
                    raise ValueError("Invalid Mach-O minimum version")
                minimum = struct.unpack_from("<I", data, position + 8)[0]
            position += size
        if position != 32 + command_bytes or minimum is None:
            raise ValueError("Missing or inconsistent Mach-O deployment metadata")
        return {"format": "Mach-O64", "architecture": architecture,
                "binary_sha256": hashlib.sha256(data).hexdigest(),
                "deployment_target": f"{minimum >> 16}.{(minimum >> 8) & 255}.{minimum & 255}"}
    raise ValueError("Unsupported native platform")
