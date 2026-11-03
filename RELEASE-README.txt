Kerf 1.0.0 (Windows x64)
========================

kerf (n.): the slit a blade leaves when it cuts.

Kerf is a small, fast video editor. You can cut parts out of a video, mute all
or part of its audio, and compress it, with optional downscaling and a target
file size.


REQUIREMENTS
------------
- Windows 10 or 11, 64-bit.
- A graphics driver with OpenGL 2.0 or newer (any normal PC has this).
- ffmpeg, which is not included. Install it once from a terminal:

      winget install Gyan.FFmpeg

  Or download it from https://www.gyan.dev/ffmpeg/builds/ and put ffmpeg.exe
  and ffprobe.exe in this folder, next to kerf.exe.

  If Kerf starts before ffmpeg is installed, install it and then click
  "Check again". You don't need to restart Kerf.
- Optional: an NVIDIA GPU (GTX 900 series or newer) enables the much faster
  GPU (NVENC) encoder.

Kerf doesn't install anything and needs no admin rights. Run kerf.exe from any
folder. To uninstall, delete the folder.


USAGE
-----
To open a video, use "Open video…", drag the file onto the window, or drag it
onto kerf.exe. From the command line, `kerf.exe video.mp4 --at 12.5` opens the
video with the playhead at 12.5 seconds. The Glance video player uses this for
its "Open in Kerf" button.

  Space            play / pause
  Click/drag       seek on the timeline
  Mouse wheel      zoom the timeline (Shift+wheel pans, "Fit" resets)
  I / O            set selection in / out point
  Del              remove the selection (video and audio)
  M                mute only the audio in the selection
  Esc              clear the selection
  Left/Right       step 0.1 s (hold Shift for 1 s)

Removed parts show red. Muted audio shows grey in the waveform. To restore
either, click ✖ on its chip.

Export modes:
  Fast        Instant and lossless. Cuts snap to the nearest keyframe, so they
              can be up to a second or two off. Keeps the original container
              and codecs.
  Compress    H.264 MP4. Cuts are frame-exact. You can choose:
              - Encoder: CPU (x264), which gives the smallest files, or
                GPU (NVENC), which is fast.
              - Resolution: Original, 1080p, 720p or 480p.
              - Target file size: aims for a size in MB, using two-pass
                encoding.


FILE FORMATS
------------
Kerf opens anything your ffmpeg can decode that has a video track. That
includes MP4, MOV, MKV, WebM, AVI, WMV, FLV, MPEG-TS/M2TS, 3GP and MPG, with
codecs such as H.264, H.265/HEVC, VP8/VP9, AV1, ProRes, MPEG-2/4 and DNxHD.
Phone videos recorded in portrait show the right way up.

Output:
  Compress    always .mp4: H.264 video and AAC audio (plays everywhere).
  Fast        same container and codecs as the source. If you muted part of
              the audio, that audio is re-encoded: to AAC, or to Opus for
              WebM/Ogg.

Known limits:
- Audio-only files and still images can't be opened.
- HDR or 10-bit sources are converted to standard 8-bit by Compress. There is
  no tone mapping, so colours can look washed out. Fast mode keeps HDR.
- Only the first video track and the first audio track are kept.
  Subtitles, chapters and extra tracks are dropped.
- Compress removes variable frame rate (VFR) timing from phone recordings, but
  audio and video stay in sync.


TROUBLESHOOTING
---------------
"ffmpeg not found": install ffmpeg (see Requirements), then click Check again.
No sound during playback: check that Windows has a default audio output
device. Kerf still plays the video, silently.
The GPU encoder is greyed out: no working NVIDIA encoder was found. Update
your NVIDIA driver, or use the CPU encoder.


SOURCE & LICENSE
----------------
Kerf is open source under the GNU GPL v3 or later (see LICENSE.txt).
Source code and new releases: https://github.com/TheSnapped/Kerf
Kerf was built with the help of AI (Claude by Anthropic).
