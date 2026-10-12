LOCAL AI HUB 0.1.0 for Windows 11 x64 -- READ ME FIRST
=====================================================

What this is
------------
Local AI Hub runs a large language model on your own PC with the Colibri engine
(https://github.com/JustVugg/colibri, used unmodified; this is NOT an official
Colibri release), gives you a terminal chat, a key-protected local API that other
programs can use, and local file workspaces.

Honest status of this download:
  * It is a command-line program (hubd.exe, started through hub.cmd). There is
    no graphical window yet.
  * It is NOT code-signed. Windows or your antivirus may warn about it. Before
    unzipping, compare the zip's SHA-256 with the value published next to the
    download (PowerShell: Get-FileHash LocalAIHub-0.1.0-windows-x64.zip) and only
    continue if you trust where you got it. SHA256SUMS.txt inside the folder only
    shows whether a file was damaged; anyone who changed the files could change it too.
  * hubd.exe was cross-compiled and tested under Wine and on Linux with the real
    Colibri engine. It has NOT yet been run on a real Windows PC. Please report
    anything that fails.
  * Sharing over the internet is off unless you explicitly turn it on.

Everything below happens in a Command Prompt opened in this folder
(in Explorer: click the address bar, type cmd, press Enter).


STEP 1  Install Python 3.10 or newer (needed by the Colibri engine)
-------------------------------------------------------------------
Download from https://www.python.org/downloads/ and tick "Add python.exe to PATH".
(Or put an unzipped Python into a folder named "python" next to hub.cmd.)


STEP 2  Add the Colibri engine
------------------------------
1. Open https://github.com/JustVugg/colibri/releases/tag/v2.0.0
2. Download  colibri-v2.0.0-windows-x86_64.zip
   (NVIDIA RTX 30/40/50 owners can also download colibri-v2.0.0-windows-x86_64-cuda.zip
   and unzip it into the same folder afterwards.)
3. Check the download against the SHA-256 shown on that release page:
      powershell Get-FileHash colibri-v2.0.0-windows-x86_64.zip
4. Unzip it into a folder named  colibri  next to hub.cmd, so that
   colibri\openai_server.py exists (a single extra folder level is fine too).
5. Run:   hub.cmd check
   It must say "Ready." It shows which Python and Colibri it found.


STEP 3  Get a model and add it
------------------------------
The Hub does not download models by itself. Use Colibri's own setup, which shows
the size and asks before downloading anything:

   colibri\coli.cmd setup --list
   colibri\coli.cmd setup --model qwen3-coder-30b --dir D:\colibri-models --no-start

(If the zip added its own folder level, use colibri\<that folder>\coli.cmd.)

(qwen3-coder-30b is about 19 GB to download and wants 8-18 GB of RAM, per the
Colibri README; --list shows what fits your PC. Check each model's own licence.)
If you already have a supported model folder, skip the download.

Then register the folder (nothing is copied or moved):

   hub.cmd init
   hub.cmd model validate D:\colibri-models\<model folder>
   hub.cmd model add      D:\colibri-models\<model folder> --id my-model

Only text-chat models that the pinned Colibri version supports are accepted.


STEP 4  Start serving (keep this window open)
---------------------------------------------
   hub.cmd key create "me"           <- copy the lah_... secret now, it is shown once
   hub.cmd serve --model my-model

Wait for "local API ready". Loading a big model can take minutes.
Serving stops when you close this window or press Ctrl+C.


STEP 5  Chat
------------
Open a SECOND Command Prompt in this folder:

   hub.cmd chat

It asks for your lah_... key: paste it and press Enter (the key is not shown while
you type or paste). To skip that question in this window, first run
   set HUB_API_KEY=lah_...your key...
If serve runs on another port (--port 8788): hub.cmd chat --url http://127.0.0.1:8788/v1

Commands inside chat: /new (fresh conversation), /system TEXT, /exit.
Ctrl+C stops a reply; at the > prompt it ends the chat. Conversations are saved
locally unless you run "hub.cmd history off" or start with "hub.cmd chat --no-history"
(see "hub.cmd history status" and "hub.cmd history list").


STEP 6  Use the model from another program
------------------------------------------
   Base URL : http://127.0.0.1:8787/v1      (this PC only)
   Model ID : my-model
   API key  : Authorization: Bearer lah_...

It speaks OpenAI-style Chat Completions (GET /v1/models, POST /v1/chat/completions,
streaming supported). It is NOT full OpenAI compatibility: see docs\API.md.
Ready-made examples are in the examples folder (Python, Node.js, PowerShell, curl).
More keys: hub.cmd key create NAME --expires-days 90 ; remove with hub.cmd key revoke ID.


STEP 7  Save files locally
--------------------------
   hub.cmd workspace create "Project A" --quota-mb 500
   hub.cmd workspace put "Project A" C:\path\report.pdf
   hub.cmd workspace files "Project A"
   hub.cmd workspace get "Project A" report.pdf C:\Users\me\Desktop\report.pdf
   hub.cmd workspace export "Project A" C:\Users\me\Desktop\project-a.zip
get and export never replace an existing file; add --overwrite to replace it.
Files are stored under %LOCALAPPDATA%\LocalAIHub\workspaces. API keys cannot reach them.


Sharing over the internet (optional, off by default)
----------------------------------------------------
Needs your own domain on Cloudflare and a named tunnel; see docs\SHARING.md.
It is untested and publishes your machine, so read that file first.
To stop sharing, stop "hub.cmd serve" (Ctrl+C). Local use does not need any of it.


If something goes wrong
-----------------------
  hub.cmd check                       what was found
  "Not enough memory ..."             close programs, pick a smaller model, or set
                                      RAM_GB=<gigabytes> before hub.cmd serve
  "port 8787 on 127.0.0.1 is not      hub.cmd serve --model my-model --port 8788
   available"                         (then: hub.cmd chat --url http://127.0.0.1:8788/v1)
  "Terminate batch job (Y/N)?"        Command Prompt asks this after Ctrl+C was pressed
                                      while hub.cmd ran; Y and N are both fine
  model validate says "not supported" that model family/format is not served by
                                      this Colibri version (GGUF is not supported)
Full documentation: docs\ (WINDOWS.md, API.md, SECURITY.md, STORAGE.md, VERIFICATION.md).


Removing it
-----------
Delete this folder. Your keys, history and workspaces stay in
%LOCALAPPDATA%\LocalAIHub until you delete that folder too. Models are never deleted
by the Hub.
