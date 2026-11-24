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
- Ctrl+C: copy the image
- E: toggle editing, with toolbar buttons and keys to crop (C), rotate (R and
  Shift+R), flip (H and V), adjust tone (T), resize (Z) and export (X)
- While editing, Enter applies a crop, Escape or right click cancels or leaves,
  Ctrl+Z and Ctrl+Shift+Z undo and redo, Ctrl+S saves a copy next to the image
  and Ctrl+Shift+S overwrites it

## Usage

Use the same Perch build for the daemon and client. Start the daemon once.

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

HEIC/HEIF, JPEG XL, OpenEXR, Farbfeld, DDS, PCX, SGI, ICNS and XPM are also
supported. Animation includes GIF, APNG, WebP, AVIF, HEIF and JPEG XL. Camera
RAW images use LibRaw's camera support and processing. PSD/PSB and OpenRaster
display their saved composites. Photoshop files without a saved composite rely
on the decoder's layer compositing. XCF is unsupported. OpenRaster, OTB, WBMP and
XBM need a matching filename extension.

Embedded RGB and grayscale ICC profiles are converted to sRGB. JPEG and Photoshop
also convert embedded CMYK profiles. Display output remains 8-bit SDR.

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

Import and enable the module in your NixOS configuration:

```nix
{inputs, ...}: {
  imports = [inputs.perch.nixosModules.default];

  # Optionally add cache settings for pre-built binaries.
  nix.settings = {
    extra-substituters = ["https://greyxp1.cachix.org"];
    extra-trusted-public-keys = ["greyxp1.cachix.org-1:hUnQC8sjfKykPgkFdcXiuv4xjiISuWnBnPfW2F2df3k="];
  };

  programs.perch.enable = true;
}
```

For Home Manager, import `inputs.perch.homeModules.default` and set
`programs.perch.enable = true`. Add the optional cache settings to your system's
Nix configuration.
