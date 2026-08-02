# Perch

`perch` is a small native Wayland image viewer for keeping reference images
on screen. It uses one resident daemon for any number of independent windows.

## Controls

- Left drag: move the window
- Mouse wheel: resize the window
- Right click: close the window

## Usage

Start the daemon once:

```sh
perch --daemon
```

Then open one or more images:

```sh
perch image.png another.webp
```

Or pipe an image through standard input:

```sh
cat image.png | perch --stdin
```

## Home Manager

Import `perch.homeModules.default`, then enable the program:

```nix
{
  imports = [inputs.perch.homeModules.default];
  programs.perch.enable = true;
}
```
