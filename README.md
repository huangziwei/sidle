# Sidle: Sideload books into your (jailbroken) Kindle.

<p align="center">
  <img src=".github/assets/Sidle-Tauri.png" height="300" />
  <img src=".github/assets/Sidle-KUAL.png" height="300" />
</p>

## TL;DR

Sidle contains two parts:

1. a Rust/Tauri app for managing books and converting various formats to EPUB and KFX on MacOS;
2. a KUAL app to pull books from from the library via WIFI.

The Tauri app doesn't require the Kindle being jailbroken, but the second one does (to instal KUAL, to begin with). 

## Build

```sh
git clone https://github.com/huangziwei/sidle
./build.sh
```

The app will be built and put into `/Applications/Sidle`

Book data and library database will be stored in `~/Library/Application Support/sidle/`

To install the KUAL app, plug in the Kindle via USB, then in the Kindle tab, enter `KUAL extension`, click `push KUAL`. 

Currenty only tested on MacOS 26 and Kindle Oasis 2 (9th Gen) with 15.16.2.1.1.

## But Why?

To sideload books to Kindle, one might just use Send-to-Kindle. But I don't want to use more Amazon services. 

One can also use Calibre. I mostly use it for DeDRM. But as Amazon tightened up their DRM recently, DeDRM stopped stripping DRM from books pulled from e-ink Kindle already. The only way to deDRM that still works for me today is to use [KFXArchiver](https://github.com/Satsuoni/DeDRM_tools/discussions/74#discussioncomment-17034265) on jailbroken devices, get the KFX-ZIP, then drag them back to Calibre to produce KFX, which can then be converted to EPUB or other formats. It's a cumbersome and slow process.

I want a faster, mor streamline process to manage my books:

1. mount the Kindle, all KFX-ZIP will be auto-imported and converted to KFX, and EPUB;
2. imported EPUB will be auto-converted to KFX;
3. imported AZW3 and MOBI will be auto-converted to EPUB then KFX;
4. (bonus) imported ZIP from [Aozora/青空文庫](https://www.aozora.gr.jp) will be auto-converted to EPUB then KFX;
5. while mouted, the desktop app can push a KUAL app to Kinde, which can be used to view the library and download the KFX from the host within the same network;
6. and most importantly, all format conversion should work for CJK typography (vertical writing mode, page progression direction, etc.), which made possible with `boko-kai`, a [fork of boko](./boko-kai/README.md).

This is basically what Sidle does for now.

## Bonus

for format conversion only without library management, you can use this [client-side html tool](https://hzwei.dev/tools/boko.html).