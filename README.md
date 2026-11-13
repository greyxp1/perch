# Perch

`perch` is a small native Wayland image viewer for keeping reference images
on screen. It uses one resident daemon for any number of independent windows.
Your compositor must support xdg-shell v4 and viewporter.

## Controls

- Left drag: move the window
- Mouse wheel: resize the window
- Right click: close the window
- Left/right arrows: previous/next image
- Space: play or pause animation
- Tab: toggle inspection, where left drag pans and the wheel zooms
- Home: fit the image
- End: show at original size

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

Standard input and each decoded image are limited to 256 MiB. Perch loads images
in the background and keeps one next decoded image up to 64 MiB. Animation is
opt-in, with a 16 MiB frame limit.

Supported tiled TIFFs read only the visible region and can use reduced-resolution
images in the main TIFF directory chain. Large tiled images without an overview
open in inspection mode. Large JPEGs start with a reduced decode and load more
detail when zoomed. Other formats decode the whole image.

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
