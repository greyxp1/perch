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

## Nix

Add Perch to your flake inputs:

```nix
inputs.perch.url = "github:greyxp1/perch";
```

Import and enable the module in your NixOS or Home Manager configuration:

```nix
{inputs, ...}: {
  # Use homeModules.default for Home Manager.
  imports = [inputs.perch.nixosModules.default];
  programs.perch.enable = true;
}
```
