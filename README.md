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

To instal the KUAL app, plug in the Kindle via USB, then in the Kindle tab, enter `KUAL extension`, click `push KUAL`. 