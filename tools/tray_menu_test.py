#!/usr/bin/env python3
"""Lista y activa el menú de la bandeja (StatusNotifierItem/dbusmenu).

Waybar activa los ítems del menú de bandeja por D-Bus de sesión; este banco
hace lo mismo para poder verificar el menú sin clics gráficos: vuelca el
árbol con sus ids y pulsa ítems por etiqueta (buscaría también dentro de
submenús, porque el árbol se recorre entero).

Uso:
  python tools/tray_menu_test.py list [PID]
  python tools/tray_menu_test.py click [PID] ETIQUETA

Con PID se elige el ícono de ese proceso si hubiera varios registrados.
Requiere python-dbus y una sesión con bus D-Bus (Wayland o X11).
"""

import sys

import dbus

BUS_NAME_PREFIX = "org.kde.StatusNotifierItem"
MENU_IFACE = "com.canonical.dbusmenu"
PROPS_IFACE = "org.freedesktop.DBus.Properties"
SNI_IFACE = "org.kde.StatusNotifierItem"


def sni_name(pid):
    bus = dbus.SessionBus()
    names = [n for n in bus.list_names() if n.startswith(BUS_NAME_PREFIX)]
    if pid:
        names = [n for n in names if f"-{pid}-" in n]
    if not names:
        sys.exit("no hay ningún StatusNotifierItem registrado en el bus")
    return sorted(names)[0]


def menu_interface(name):
    bus = dbus.SessionBus()
    sni = bus.get_object(name, "/StatusNotifierItem")
    path = sni.Get(SNI_IFACE, "Menu", dbus_interface=PROPS_IFACE)
    return dbus.Interface(bus.get_object(name, path), MENU_IFACE)


def walk(node, out, depth=0):
    nid, props, children = node
    label = str(props.get("label", ""))
    sub = str(props.get("children-display", "")) == "submenu"
    out.append((int(nid), label, depth, sub))
    for child in children:
        walk(child, out, depth + 1)


def main():
    args = sys.argv[1:]
    cmd = args.pop(0) if args else "list"
    pid = None
    if args and args[0].isdigit():
        pid = args.pop(0)

    name = sni_name(pid)
    menu = menu_interface(name)
    items = []
    _revision, root = menu.GetLayout(0, -1, [])
    walk(root, items)

    if cmd == "list":
        print(f"SNI: {name}")
        for nid, label, depth, sub in items:
            print(f"{'  ' * depth}{nid:>4}  {label}{'  >' if sub else ''}")
        return

    if cmd == "click":
        if not args:
            sys.exit("falta la etiqueta del ítem")
        etiqueta = args[0].lower()
        for nid, label, _depth, _sub in items:
            if etiqueta in label.lower():
                menu.Event(nid, "clicked", "", 0)
                print(f"[ok] pulsado id={nid} «{label}»")
                return
        sys.exit(f"no se encontró ningún ítem con «{args[0]}»")

    sys.exit(f"orden desconocida: {cmd} (usa list o click)")


if __name__ == "__main__":
    main()
