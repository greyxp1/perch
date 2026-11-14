# Perch

`perch` is a small native Wayland image viewer for keeping reference images
on screen. It uses one resident daemon for any number of independent windows.
Your compositor must support xdg-shell v4 and viewporter.

## Controls

- Left drag: move the window
- Mouse wheel: resize the window
- Right click or Q: close the window
- Left/right arrows: previous/next image, hold to repeat
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

Perch loads images in the background and keeps one next decoded image up to
64 MiB. Animation is opt-in. Input, decoding and playback have no memory cap.
Temporary render regions remain bounded, and each pixel buffer must fit
Wayland's 2 GiB limit. Large images stay within the window when showing original
size, with inspection enabled for panning.

Supported TIFFs read the visible tiles or strips and can use reduced-resolution
images in the main directory chain or SubIFDs. Large TIFFs without an overview
build a fitted preview. Large PNGs stream scanlines into a preview and rescan for
detail. JPEG and static WebP use reduced previews and decode regions when zoomed.
Other formats can still decode the whole image, and some codecs require workspace
that grows with image size. Piped input uses temporary storage.

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
