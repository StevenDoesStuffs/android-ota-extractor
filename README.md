# android-ota-extractor

A tool to extract images from and inspect the payload.bin file from Android OTA zips.

### Features
- Display partition information and update operations
- Extract `img` files from full OTAs
- Apply incremental OTAs given old images
- Hash checking for old images and payload data
- Support for bsdiff and (TODO) puffdiff operations

## Requirements

Use linux, and have [bzip2](https://archlinux.org/packages/core/x86_64/bzip2/) and [brotli](https://archlinux.org/packages/core/x86_64/brotli/) installed.

If you're on Windows, I'd greatly appreciate some help on getting this to work.
I'm not even sure that it won't work since I don't have Windows!
One important part is to get the dynamically linked libraries to work.

## Usage

```
$ android-ota-extractor --help
A program to extract image files from a payload.bin OTA file

Usage: android-ota-extractor <COMMAND>

Commands:
  extract  Extract image files from the payload file
  inspect  List image files included in the payload file
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```
```
$ android-ota-extractor inspect --help
Show information about included partition updates

Usage: android-ota-extractor inspect [OPTIONS] <FILE>

Arguments:
  <FILE>  The payload.bin file

Options:
      --dump-ops [<DUMP_OPS>]  The parts to list operations for; leave empty for all parts
  -h, --help                   Print help

```
```
$ android-ota-extractor extract --help
Extract image files from the payload file

Usage: android-ota-extractor extract [OPTIONS] --dst <DST> <FILE>

Arguments:
  <FILE>  The payload.bin file

Options:
      --src <SRC>        The folder which contains the image files before the update (only needed for incremental OTAs)
      --dst <DST>        The folder which will contain the image files after the update
      --parts [<PARTS>]  The parts to extract; defaults to all parts
      --skip-hash        Disable hash checking for src images and payload data
  -h, --help             Print help
```

## Technical Details

Modern android OTAs are a zip file containing a payload.bin which stores all the information about the update.
Some updates are full updates, and others are incremental.
Typically full updates are gigabytes big, and incremental updates are much smaller.
The data for an update is contained in a `DeltaArchiveManifest`,
which contains a list of `PartitionUpdate`s.

Each `PartitionUpdate` contains the information needed on how to update a single partition
and contains a list of operations to create a new image which will contain the updated software.
Importantly, due to [A/B partitioning](https://source.android.com/docs/core/ota/ab), this is not an in-place update!
Typical operations include: writing new data contained in the payload, applying a patch in the payload to the old image, and copying from the old image to the new image.

For more details, see `src/update_metadata.proto`.
