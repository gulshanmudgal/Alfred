using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Windows.Automation;

internal sealed record HostRequest(
    string Id,
    string Method,
    string? CapabilityToken,
    JsonElement? Params,
    string? Application,
    string? Intent,
    string? Target);

internal static partial class Program
{
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);
    private static readonly string ExpectedToken = Environment.GetEnvironmentVariable("ALFRED_CAPABILITY_TOKEN") ?? "";
    // Stable Windows inbox/browser aliases. Other applications are launchable
    // only when an exact Start-menu or AppsFolder name is installed; the planner
    // can never supply an executable path or arbitrary command line.
    private static readonly IReadOnlyDictionary<string, string> LaunchTargets = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase)
    {
        ["Notepad"] = "notepad.exe",
        ["Calculator"] = "calc.exe",
        ["Paint"] = "mspaint.exe",
        ["File Explorer"] = "explorer.exe",
        ["Microsoft Edge"] = "msedge.exe",
        ["Google Chrome"] = "chrome.exe",
        ["Brave"] = "brave.exe",
        ["Windows Terminal"] = "wt.exe",
        ["Microsoft Store"] = "ms-windows-store:",
        ["Settings"] = "ms-settings:"
    };
    private static readonly HashSet<string> BrowserApplications = new(StringComparer.OrdinalIgnoreCase)
    {
        "Microsoft Edge", "Google Chrome", "Brave"
    };

    // Keystrokes Alfred may send: Backspace, Tab, Enter, Escape, Space, PageUp/Down,
    // End, Home, arrow keys, and F1-F12. Deletion (VK_DELETE) and every unlisted key
    // are refused so raw virtual-key codes can never bypass the semantic safety policy.
    private const int VK_DELETE = 0x2E;
    private static readonly HashSet<int> AllowedVirtualKeys =
    [
        0x08, 0x09, 0x0D, 0x1B, 0x20,
        0x21, 0x22, 0x23, 0x24,
        0x25, 0x26, 0x27, 0x28,
        0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x7B
    ];
    private static readonly IReadOnlyDictionary<string, ushort> AllowedShortcuts =
        new Dictionary<string, ushort>(StringComparer.OrdinalIgnoreCase)
        {
            ["CTRL+L"] = 0x4C, // Browser / Explorer address bar
            ["CTRL+S"] = 0x53  // Save / Save As
        };

    private const int SW_SHOW = 5;
    private const int SW_RESTORE = 9;
    private static readonly IntPtr HWND_TOPMOST = new(-1);
    private static readonly IntPtr HWND_NOTOPMOST = new(-2);
    private const uint SWP_NOSIZE = 0x0001;
    private const uint SWP_NOMOVE = 0x0002;
    private const uint SWP_SHOWWINDOW = 0x0040;
    private const uint PW_RENDERFULLCONTENT = 0x00000002;
    // The Rust planner keeps at most 40 interesting controls from an observation.
    // Bound the raw UIA snapshot as well: modern Windows apps can expose thousands
    // of descendants, and writing that JSON as one stdout frame can exceed the
    // Windows anonymous-pipe limit (Win32 error 223) and poison later actions.
    private const int MaxSnapshotNodes = 120;
    private const int MaxSnapshotChildren = 60;
    private const int MaxSnapshotTextChars = 256;

    [STAThread]
    private static async Task Main(string[] args)
    {
        // Rust and every supported planner CLI speak UTF-8 over redirected
        // pipes. Windows can otherwise inherit an OEM console code page and
        // turn em dashes/emoji into mojibake such as ΓÇö / ≡ƒÉÖ.
        try { System.Windows.Forms.Application.SetHighDpiMode(HighDpiMode.PerMonitorV2); } catch { /* Console host. */ }
        SetProcessDpiAwarenessContext(DpiAwarenessContextPerMonitorV2);
        Console.InputEncoding = new UTF8Encoding(false, true);
        Console.OutputEncoding = new UTF8Encoding(false, true);
        if (args.Any(value => value.StartsWith("chrome-extension://", StringComparison.OrdinalIgnoreCase)) || args.Contains("--browser-bridge"))
        {
            await RunBrowserBridge();
            return;
        }
        string? line;
        while ((line = await Console.In.ReadLineAsync()) is not null)
        {
            HostRequest? request = null;
            try
            {
                request = JsonSerializer.Deserialize<HostRequest>(line, Json) ?? throw new InvalidOperationException("Empty request");
                Authorize(request);
                var result = Dispatch(request);
                Reply(new { id = request.Id, ok = true, result });
            }
            catch (Exception error)
            {
                Reply(new { id = request?.Id ?? "unknown", ok = false, error = error.Message });
            }
        }
    }

    private static async Task RunBrowserBridge()
    {
        var root = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Alfred");
        Directory.CreateDirectory(root);
        var tokenPath = Path.Combine(root, "browser-bridge-token");
        if (!File.Exists(tokenPath)) await File.WriteAllTextAsync(tokenPath, Convert.ToHexString(RandomNumberGenerator.GetBytes(32)));
        var token = (await File.ReadAllTextAsync(tokenPath)).Trim();
        var listener = new TcpListener(IPAddress.Loopback, 17844);
        listener.Start(1);
        while (true)
        {
            using var client = await listener.AcceptTcpClientAsync();
            using var network = client.GetStream();
            using var reader = new StreamReader(network, leaveOpen: true);
            using var writer = new StreamWriter(network, leaveOpen: true) { AutoFlush = true };
            var line = await reader.ReadLineAsync();
            if (line is null) continue;
            using var envelope = JsonDocument.Parse(line);
            if (!envelope.RootElement.TryGetProperty("capabilityToken", out var supplied) || supplied.GetString() != token)
            {
                await writer.WriteLineAsync("{\"ok\":false,\"error\":\"Invalid browser bridge token\"}");
                continue;
            }
            var request = envelope.RootElement.GetProperty("request").GetRawText();
            var bytes = System.Text.Encoding.UTF8.GetBytes(request);
            var output = Console.OpenStandardOutput();
            await output.WriteAsync(BitConverter.GetBytes(bytes.Length));
            await output.WriteAsync(bytes);
            await output.FlushAsync();
            var input = Console.OpenStandardInput();
            var header = new byte[4];
            await input.ReadExactlyAsync(header);
            var response = new byte[BitConverter.ToInt32(header)];
            await input.ReadExactlyAsync(response);
            await writer.WriteLineAsync(System.Text.Encoding.UTF8.GetString(response));
        }
    }

    private static void Authorize(HostRequest request)
    {
        if (string.IsNullOrWhiteSpace(ExpectedToken) || request.CapabilityToken != ExpectedToken)
            throw new UnauthorizedAccessException("Invalid or missing Alfred capability token.");
        // Persistent data-loss only. Do not scan type/value payloads — a user
        // asking to "remove a filter" or retype after deleting a draft is legal.
        if (IsPersistentDataLoss(request.Method, request.Intent, request.Target, request.Params))
            throw new UnauthorizedAccessException("Destructive actions are blocked by the Windows host.");
    }

    private static object Dispatch(HostRequest request) => request.Method switch
    {
        "health" => new { host = "windows", version = "0.4.0", processId = Environment.ProcessId },
        "listApplications" => ListApplications(),
        "listInstalledApplications" => ListInstalledApplications(request),
        "resolveApplication" => ResolveApplication(GetString(request.Params, "name")),
        "launchApplication" => LaunchApplication(request),
        "focusApplication" => FocusApplication(request),
        "navigateApplication" => NavigateApplication(request),
        "activate" => Activate(ResolveProcess(request).Id),
        "observeWindow" => ObserveWindow(request),
        "captureWindow" => CaptureWindow(request),
        "findElement" => FindElementInfo(request),
        "getValue" => GetElementValue(request),
        "invokeElement" => InvokeElement(request),
        "setValue" => SetElementValue(request),
        "click" => Click(request),
        "typeText" => TypeText(request, GetString(request.Params, "text")),
        "key" => PressKey(request, GetInt(request.Params, "virtualKey")),
        "shortcut" => PressShortcut(request, GetString(request.Params, "keys")),
        "probe" => Probe(request),
        "scroll" => Scroll(request),
        "rightClick" => PointerGesture(request, "rightClick"),
        "doubleClick" => PointerGesture(request, "doubleClick"),
        "hover" => PointerGesture(request, "hover"),
        "drag" => Drag(request),
        "wait" => WaitFor(request),
        _ => throw new InvalidOperationException($"Unsupported host method: {request.Method}")
    };

    private static object ListApplications()
    {
        var items = new List<(int id, string name, string title)>();
        var seen = new HashSet<int>();
        foreach (var window in EnumerateTopWindows())
        {
            try
            {
                using var process = Process.GetProcessById(window.ProcessId);
                var processName = process.ProcessName ?? "";
                if (processName.StartsWith("alfred", StringComparison.OrdinalIgnoreCase)) continue;
                if (!seen.Add(window.ProcessId)) continue;
                items.Add((window.ProcessId, processName, window.Title));
            }
            catch { /* The process exited or denies access; skip it. */ }
        }
        PruneDeadMarks();
        return items.OrderBy(item => item.name)
            .Take(200)
            .Select(item => new { processId = item.id, name = item.name, title = Truncate(item.title, 160) })
            .ToArray();
    }

    private static object FocusApplication(HostRequest request)
    {
        var process = ResolveProcess(request);
        var application = GetApplication(request);
        FocusProcess(process, application);
        return new
        {
            focused = true,
            application,
            processId = process.Id,
            title = Truncate(WindowTitle(FindBestWindowHandle(process, application)), 160)
        };
    }

    private static object NavigateApplication(HostRequest request)
    {
        var application = GetApplication(request);
        if (!BrowserApplications.Contains(application))
            throw new UnauthorizedAccessException("Native URL navigation is restricted to an allow-listed browser application.");
        var url = GetString(request.Params, "url").Trim();
        if (url.Length is 0 or > 2048
            || url.Any(char.IsControl)
            || !Uri.TryCreate(url, UriKind.Absolute, out var parsed)
            || (parsed.Scheme != Uri.UriSchemeHttps && parsed.Scheme != Uri.UriSchemeHttp)
            || !string.IsNullOrEmpty(parsed.UserInfo))
            throw new UnauthorizedAccessException("Native browser navigation accepts only an absolute HTTP(S) URL up to 2048 characters.");
        var process = ResolveProcess(request);
        FocusProcess(process, application);
        var addressBar = FindBrowserAddressBar(process);
        addressBar.SetFocus();
        Thread.Sleep(100);
        var focused = AutomationElement.FocusedElement;
        if (focused is null || !ElementBelongsToProcess(focused, process, application) || !IsBrowserChromeEditor(focused, process))
            throw new InvalidOperationException("The browser address bar did not receive focus; navigation was not sent.");
        var usedDirectValue = false;
        if (addressBar.TryGetCurrentPattern(ValuePattern.Pattern, out var pattern)
            && !((ValuePattern)pattern).Current.IsReadOnly)
        {
            ((ValuePattern)pattern).SetValue(url);
            usedDirectValue = true;
        }
        else
        {
            const ushort control = 0x11;
            const ushort letterA = 0x41;
            Send([
                VirtualKeyInput(control, false),
                VirtualKeyInput(letterA, false),
                VirtualKeyInput(letterA, true),
                VirtualKeyInput(control, true)
            ]);
            HumanTypeCharacters(url, process.Id);
        }
        Thread.Sleep(100);
        var enteredUrl = ReadElementText(addressBar);
        if (!string.Equals(NormalizeText(enteredUrl), NormalizeText(url), StringComparison.OrdinalIgnoreCase))
            throw new InvalidOperationException("The requested URL was not present in the browser address bar; navigation was not submitted.");
        Send([VirtualKeyInput(0x0D, false), VirtualKeyInput(0x0D, true)]);
        return new
        {
            navigated = true,
            verifiedAddressBar = true,
            usedDirectValue,
            application,
            url
        };
    }

    private static object Activate(int processId)
    {
        var process = Process.GetProcessById(processId);
        FocusProcess(process);
        return new { activated = true, processId, title = Truncate(WindowTitle(FindBestWindowHandle(process, null)), 160) };
    }

    // Brings the target window to the foreground and proves it stayed there before
    // any input is sent. UIA SetFocus is attempted first; SetForegroundWindow is
    // retried while Windows settles.
    private static void FocusProcess(Process process, string? application = null)
    {
        process.Refresh();
        RememberApplication(process.Id, application);
        var handle = RequireWindowHandle(process, application);
        if (IsIconic(handle)) ShowWindow(handle, SW_RESTORE);
        var deadline = DateTime.UtcNow.AddSeconds(5);
        var lastAttempt = DateTime.MinValue;
        while (DateTime.UtcNow < deadline)
        {
            var foreground = GetForegroundWindow();
            GetWindowThreadProcessId(foreground, out var foregroundPid);
            GetWindowThreadProcessId(handle, out var handlePid);
            var foregroundTitle = WindowTitle(foreground);
            if (ForegroundIsTarget(handle, process.Id, application, foreground, foregroundPid, handlePid, foregroundTitle))
            {
                Thread.Sleep(150);
                return;
            }
            if ((DateTime.UtcNow - lastAttempt).TotalMilliseconds > 750)
            {
                ForceForegroundWindow(handle);
                lastAttempt = DateTime.UtcNow;
            }
            Thread.Sleep(100);
        }
        var leftover = GetForegroundWindow();
        GetWindowThreadProcessId(leftover, out var leftoverPid);
        throw new InvalidOperationException(
            $"Could not bring the target window to the foreground (wanted '{Truncate(WindowTitle(handle), 120)}' pid {process.Id}; foreground '{Truncate(WindowTitle(leftover), 120)}' pid {leftoverPid}).");
    }

    private static bool ForegroundIsTarget(
        IntPtr handle,
        int processId,
        string? application,
        IntPtr foreground,
        uint foregroundPid,
        uint handlePid,
        string foregroundTitle)
    {
        if (foreground == IntPtr.Zero) return false;
        if (foreground == handle) return true;
        if (handle != IntPtr.Zero && GetAncestor(foreground, GA_ROOT) == handle) return true;
        // ApplicationFrameHost hosts many UWP frames under one PID. PID match
        // alone would treat Settings as Store. Require the exact frame or title.
        var sharedFrame = IsApplicationFrameHost(processId)
            || (handlePid != 0 && IsApplicationFrameHost((int)handlePid));
        if (!sharedFrame)
        {
            if (foregroundPid == (uint)processId) return true;
            if (handlePid != 0 && foregroundPid == handlePid) return true;
        }
        return ForegroundLooksLikeApplication(application, foregroundTitle);
    }

    private static bool ForegroundLooksLikeApplication(string? application, string title)
    {
        if (string.IsNullOrWhiteSpace(application) || string.IsNullOrWhiteSpace(title))
            return false;
        if (TitleMatchesApplication(title, application)) return true;
        var titleNorm = NormalizeText(title);
        var appNorm = NormalizeText(application);
        return appNorm.Length > 0 && titleNorm.Contains(appNorm, StringComparison.Ordinal);
    }

    // Windows normally prevents a background process from stealing focus. The
    // trusted host temporarily joins its input queue to the current foreground
    // and target window threads, performs the bounded focus transfer, then
    // detaches immediately. Callers still verify foreground ownership above.
    private static void ForceForegroundWindow(IntPtr handle)
    {
        var foreground = GetForegroundWindow();
        var foregroundThread = GetWindowThreadProcessId(foreground, out _);
        var targetThread = GetWindowThreadProcessId(handle, out _);
        var currentThread = GetCurrentThreadId();
        var attachedForeground = foregroundThread != 0 && foregroundThread != currentThread
            && AttachThreadInput(currentThread, foregroundThread, true);
        var attachedTarget = targetThread != 0 && targetThread != currentThread && targetThread != foregroundThread
            && AttachThreadInput(currentThread, targetThread, true);
        try
        {
            if (IsIconic(handle)) ShowWindow(handle, SW_RESTORE);
            BringWindowToTop(handle);
            SetForegroundWindow(handle);
            try { AutomationElement.FromHandle(handle)?.SetFocus(); } catch { }
            if (GetForegroundWindow() == handle) return;
            ShowWindow(handle, SW_SHOW);
            SetWindowPos(handle, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
            SetWindowPos(handle, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
            // Any synthetic input from this process resets the foreground lock.
            Send([MouseInput(MOUSEEVENTF_MOVE)]);
            SetForegroundWindow(handle);
            try { AutomationElement.FromHandle(handle)?.SetFocus(); } catch { }
        }
        finally
        {
            if (attachedTarget) AttachThreadInput(currentThread, targetThread, false);
            if (attachedForeground) AttachThreadInput(currentThread, foregroundThread, false);
        }
    }

    private static Process ResolveProcess(HostRequest request)
    {
        var application = GetApplication(request);
        var processId = GetOptionalInt(request.Params, "processId");
        if (processId.HasValue)
        {
            try
            {
                var process = Process.GetProcessById(processId.Value);
                RememberApplication(process.Id, application);
                if (FindBestWindowHandle(process, application) != IntPtr.Zero)
                    return process;
                process.Dispose();
            }
            catch { /* UWP stubs exit or hand off; fall back to the application name. */ }
        }
        var resolved = FindApplicationProcess(application)
            ?? throw new InvalidOperationException($"{application} is not open.");
        RememberApplication(resolved.Id, application);
        return resolved;
    }

    private static string GetApplication(HostRequest request) =>
        string.IsNullOrWhiteSpace(request.Application)
            ? GetOptionalString(request.Params, "application")
                ?? throw new InvalidOperationException("Missing application.")
            : request.Application.Trim();

    private static object ObserveWindow(HostRequest request)
    {
        var process = ResolveProcess(request);
        return BuildObservation(process, GetApplication(request));
    }

    private static object Snapshot(AutomationElement element, int depth, ref int remainingNodes)
    {
        remainingNodes--;
        var properties = element.Current;
        var children = new List<object>();
        if (depth < 5 && remainingNodes > 0)
        {
            var found = element.FindAll(TreeScope.Children, Condition.TrueCondition);
            foreach (AutomationElement child in found.Cast<AutomationElement>().Take(MaxSnapshotChildren))
            {
                if (remainingNodes <= 0) break;
                children.Add(Snapshot(child, depth + 1, ref remainingNodes));
            }
        }
        var bounds = properties.BoundingRectangle;
        return new
        {
            name = TruncateSnapshotText(properties.Name),
            automationId = TruncateSnapshotText(properties.AutomationId),
            controlType = properties.ControlType?.ProgrammaticName,
            enabled = properties.IsEnabled,
            offscreen = properties.IsOffscreen,
            bounds = JsonBounds(bounds),
            children = children.ToArray()
        };
    }

    private static string TruncateSnapshotText(string? value) =>
        string.IsNullOrEmpty(value) || value.Length <= MaxSnapshotTextChars
            ? value ?? ""
            : value[..MaxSnapshotTextChars];

    private static object JsonBounds(System.Windows.Rect bounds) => new
    {
        x = JsonNumber(bounds.X),
        y = JsonNumber(bounds.Y),
        width = JsonNumber(bounds.Width),
        height = JsonNumber(bounds.Height)
    };

    private static double JsonNumber(double value) => double.IsFinite(value) ? value : 0;

    private static object CaptureWindow(HostRequest request)
    {
        var process = ResolveProcess(request);
        var handle = RequireWindowHandle(process, GetApplication(request));
        if (IsIconic(handle))
            throw new InvalidOperationException("The target window is minimized; run an activate step first.");
        if (!GetWindowRect(handle, out var rect))
            throw new InvalidOperationException("Could not read the window bounds.");
        var width = Math.Max(1, rect.Right - rect.Left);
        var height = Math.Max(1, rect.Bottom - rect.Top);
        using var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        using (var graphics = Graphics.FromImage(bitmap))
        {
            // PrintWindow captures the window itself, so evidence cannot accidentally
            // include an overlapping window from another application. Some GPU-rendered
            // windows refuse it; fall back to reading the screen region for those.
            var hdc = graphics.GetHdc();
            var rendered = PrintWindow(handle, hdc, PW_RENDERFULLCONTENT);
            graphics.ReleaseHdc(hdc);
            if (!rendered)
                graphics.CopyFromScreen(rect.Left, rect.Top, 0, 0, new Size(width, height), CopyPixelOperation.SourceCopy);
        }
        if (GenerationFor(process.Id) == 0)
            BuildObservation(process, GetApplication(request));
        using var annotated = AnnotateMarks(bitmap, process, rect);
        using var output = new MemoryStream();
        annotated.Save(output, ImageFormat.Png);
        return new
        {
            mimeType = "image/png",
            width,
            height,
            annotated = true,
            generation = GenerationFor(process.Id),
            base64 = Convert.ToBase64String(output.ToArray())
        };
    }

    private static AutomationElement FindElement(Process process, string? automationId, string? name, string? controlType)
    {
        var root = RequireAutomationRoot(process, null);
        if (!string.IsNullOrEmpty(automationId))
        {
            var byId = root.FindFirst(TreeScope.Descendants,
                new PropertyCondition(AutomationElement.AutomationIdProperty, automationId));
            if (byId is not null) return byId;
            if (string.IsNullOrEmpty(name))
                throw new InvalidOperationException("The requested UI element was not found.");
        }
        if (!string.IsNullOrEmpty(name))
        {
            Condition condition = new PropertyCondition(AutomationElement.NameProperty, name);
            var type = ParseControlType(controlType);
            if (type is not null)
                condition = new AndCondition(condition, new PropertyCondition(AutomationElement.ControlTypeProperty, type));
            var byName = root.FindFirst(TreeScope.Descendants, condition);
            if (byName is not null) return byName;
        }
        else if (ParseControlType(controlType) is ControlType onlyType)
        {
            var byType = root.FindFirst(TreeScope.Descendants,
                new PropertyCondition(AutomationElement.ControlTypeProperty, onlyType));
            if (byType is not null) return byType;
        }
        throw new InvalidOperationException("The requested UI element was not found.");
    }

    private static ControlType? ParseControlType(string? value)
    {
        if (string.IsNullOrWhiteSpace(value)) return null;
        var shortName = value.Replace("ControlType.", "");
        var field = typeof(ControlType).GetField(shortName,
            System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static);
        return field?.GetValue(null) as ControlType;
    }

    // Never throws for a missing element: presence checks drive wait/precondition logic.
    private static object FindElementInfo(HostRequest request)
    {
        try
        {
            var process = ResolveProcess(request);
            var text = GetOptionalString(request.Params, "text");
            if (!string.IsNullOrWhiteSpace(text))
            {
                var matches = FindMarksByText(process, GetApplication(request), text);
                return new
                {
                    found = matches.Count > 0,
                    count = matches.Count,
                    mark = matches.FirstOrDefault()?.Id,
                    matches = matches.Select(MarkJson).ToArray()
                };
            }
            var element = ResolveTargetElement(request, requireMark: false);
            var mark = MintMark(process, GetApplication(request), element);
            var bounds = element.Current.BoundingRectangle;
            return new
            {
                found = true,
                mark = mark.Id,
                generation = mark.Generation,
                name = element.Current.Name,
                automationId = element.Current.AutomationId,
                controlType = element.Current.ControlType?.ProgrammaticName,
                bounds = JsonBounds(bounds)
            };
        }
        catch
        {
            return new { found = false, name = "", automationId = "", controlType = "", mark = (string?)null };
        }
    }

    private static object GetElementValue(HostRequest request)
    {
        var element = ResolveTargetElement(request, requireMark: false);
        if (IsPasswordElement(element))
            throw new UnauthorizedAccessException("Alfred will not read a password field.");
        if (element.TryGetCurrentPattern(ValuePattern.Pattern, out var pattern))
            return new { value = ((ValuePattern)pattern).Current.Value, mark = GetOptionalString(request.Params, "mark") };
        var name = element.Current.Name;
        if (!string.IsNullOrEmpty(name)) return new { value = name, mark = GetOptionalString(request.Params, "mark") };
        throw new InvalidOperationException("The UI element exposes no readable value.");
    }

    private static object InvokeElement(HostRequest request)
    {
        var process = ResolveProcess(request);
        FocusProcess(process, GetApplication(request));
        var element = ResolveTargetElement(request, requireMark: false);
        return InvokeViaPatterns(process, element, request.Target);
    }

    private static object SetElementValue(HostRequest request)
    {
        var value = GetString(request.Params, "value");
        var process = ResolveProcess(request);
        FocusProcess(process, GetApplication(request));
        var element = ResolveTargetElement(request, requireMark: false);
        RefusePasswordElement(element);
        if (element.TryGetCurrentPattern(ValuePattern.Pattern, out var pattern)
            && !((ValuePattern)pattern).Current.IsReadOnly)
        {
            ((ValuePattern)pattern).SetValue(value);
            Thread.Sleep(100);
            var observed = ReadElementText(element);
            if (TextAppearsExactlyOnce(observed, value))
            {
                return new
                {
                    set = true,
                    verified = true,
                    how = "setValue",
                    characters = value.Length,
                    observedText = Truncate(observed, 500),
                    targetName = element.Current.Name,
                    controlType = element.Current.ControlType?.ProgrammaticName,
                    mark = GetOptionalString(request.Params, "mark")
                };
            }
        }
        try { element.SetFocus(); } catch { }
        var bounds = element.Current.BoundingRectangle;
        if (bounds.Width > 2 && bounds.Height > 2)
            HumanLeftClick(process.Id, (int)Math.Round(bounds.Left + bounds.Width / 2), (int)Math.Round(bounds.Top + bounds.Height / 2));
        Thread.Sleep(80);
        SelectAllInFocusedControl();
        Thread.Sleep(50);
        HumanTypeCharacters(value, process.Id);
        Thread.Sleep(150);
        var typed = ReadTargetSubtreeText(element);
        if (!TextAppearsExactlyOnce(typed, value))
            throw new InvalidOperationException("The value action returned, but the requested text was not present in the target control.");
        return new
        {
            set = true,
            verified = true,
            how = "humanType",
            characters = value.Length,
            observedText = Truncate(typed, 500),
            targetName = element.Current.Name,
            controlType = element.Current.ControlType?.ProgrammaticName,
            mark = GetOptionalString(request.Params, "mark")
        };
    }

    private static readonly HashSet<string> GenericTargetWords = new(StringComparer.OrdinalIgnoreCase)
    {
        "button", "click", "control", "editable", "editor", "enabled", "field", "find",
        "input", "locate", "lower", "open", "page", "press", "submit", "upper",
        "visible", "area", "box", "text"
    };

    private static string[] TargetWords(string? value) => (value ?? "")
        .ToLowerInvariant()
        .Split(new[] { ' ', '\t', '\r', '\n', '-', '_', ':', '.', ',', '/', '(', ')' }, StringSplitOptions.RemoveEmptyEntries)
        .Where(word => word.Length >= 2 && !GenericTargetWords.Contains(word))
        .Distinct(StringComparer.OrdinalIgnoreCase)
        .ToArray();

    private static string NormalizeText(string? value) => string.Join(' ', (value ?? "")
        .ToLowerInvariant()
        .Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries)
        .Select(word => new string(word.Where(char.IsLetterOrDigit).ToArray()))
        .Where(word => word.Length > 0));

    private static bool TextMatches(string? observed, string expected)
    {
        var actual = NormalizeText(observed);
        var wanted = NormalizeText(expected);
        if (wanted.Length == 0 || actual.Length == 0) return false;
        if (actual.Contains(wanted, StringComparison.OrdinalIgnoreCase)) return true;
        var tokens = TargetWords(expected).Where(word => word.Length >= 3).Take(10).ToArray();
        if (tokens.Length == 0) return actual.Contains(wanted, StringComparison.OrdinalIgnoreCase);
        var matched = tokens.Count(token => actual.Contains(token, StringComparison.OrdinalIgnoreCase));
        return matched >= Math.Min(3, tokens.Length) && matched * 5 >= tokens.Length * 3;
    }

    private static int CountOccurrences(string haystack, string needle)
    {
        if (haystack.Length == 0 || needle.Length == 0) return 0;
        var count = 0;
        var offset = 0;
        while ((offset = haystack.IndexOf(needle, offset, StringComparison.OrdinalIgnoreCase)) >= 0)
        {
            count++;
            offset += needle.Length;
        }
        return count;
    }

    private static bool TextAppearsExactlyOnce(string? observed, string expected)
    {
        var actual = NormalizeText(observed);
        var wanted = NormalizeText(expected);
        return wanted.Length > 0 && CountOccurrences(actual, wanted) == 1;
    }

    private static void SelectAllInFocusedControl()
    {
        const ushort control = 0x11;
        const ushort letterA = 0x41;
        Send([
            VirtualKeyInput(control, false),
            VirtualKeyInput(letterA, false),
            VirtualKeyInput(letterA, true),
            VirtualKeyInput(control, true)
        ]);
    }

    private static string ReadElementText(AutomationElement element)
    {
        try
        {
            if (element.TryGetCurrentPattern(ValuePattern.Pattern, out var valuePattern))
            {
                var value = ((ValuePattern)valuePattern).Current.Value;
                if (!string.IsNullOrWhiteSpace(value)) return value;
            }
            if (element.TryGetCurrentPattern(TextPattern.Pattern, out var textPattern))
            {
                var value = ((TextPattern)textPattern).DocumentRange.GetText(2048);
                if (!string.IsNullOrWhiteSpace(value)) return value;
            }
            return element.Current.Name ?? "";
        }
        catch { return ""; }
    }

    private static string ReadTargetSubtreeText(AutomationElement target)
    {
        var values = new List<string>();
        var direct = ReadElementText(target);
        if (!string.IsNullOrWhiteSpace(direct)) values.Add(direct);
        try
        {
            var descendants = target.FindAll(TreeScope.Descendants, Condition.TrueCondition);
            foreach (AutomationElement child in descendants.Cast<AutomationElement>().Take(60))
            {
                var value = ReadElementText(child);
                if (!string.IsNullOrWhiteSpace(value)) values.Add(value);
            }
        }
        catch { /* Dynamic web controls may replace their accessibility node. */ }
        var unique = values
            .Select(value => value.Trim())
            .Where(value => value.Length > 0)
            .Distinct(StringComparer.OrdinalIgnoreCase)
            .ToList();
        if (unique.Count == 0) return "";
        // WinUI editors often echo the same document on the parent and a child.
        // Joining those copies made a single verified type look like a duplicate.
        var longest = unique.OrderByDescending(value => value.Length).First();
        var longestNorm = NormalizeText(longest);
        if (unique.All(value => longestNorm.Contains(NormalizeText(value))))
            return longest;
        return string.Join(" ", unique);
    }

    private static bool IsUsableElement(AutomationElement element)
    {
        try
        {
            var current = element.Current;
            var bounds = current.BoundingRectangle;
            return current.IsEnabled && !current.IsOffscreen && bounds.Width > 2 && bounds.Height > 2;
        }
        catch { return false; }
    }

    private static bool IsBrowserChromeEditor(AutomationElement element, Process process)
    {
        try
        {
            if (!GetWindowRect(FindBestWindowHandle(process, null), out var window)) return false;
            var current = element.Current;
            var name = (current.Name ?? "").ToLowerInvariant();
            var bounds = current.BoundingRectangle;
            return bounds.Top < window.Top + 130
                && (name.Contains("address") || name.Contains("search bar") || name.Contains("url"));
        }
        catch { return false; }
    }

    private static bool IsBrowserChromeControl(AutomationElement element, Process process)
    {
        try
        {
            if (!GetWindowRect(FindBestWindowHandle(process, null), out var window)) return false;
            var bounds = element.Current.BoundingRectangle;
            // Edge's tab strip/address toolbar lives above the web content. Do not
            // let a vision coordinate intended for the page resolve to Favorites,
            // Back, Reload, or another browser-chrome control.
            return bounds.Top < window.Top + 82;
        }
        catch { return false; }
    }

    private static AutomationElement FindBrowserAddressBar(Process process)
    {
        var root = RequireAutomationRoot(process, null);
        AutomationElement? best = null;
        var bestScore = int.MinValue;
        var descendants = root.FindAll(TreeScope.Descendants,
            new PropertyCondition(AutomationElement.ControlTypeProperty, ControlType.Edit));
        foreach (AutomationElement candidate in descendants.Cast<AutomationElement>())
        {
            try
            {
                if (!IsUsableElement(candidate) || !IsBrowserChromeEditor(candidate, process)) continue;
                var name = (candidate.Current.Name ?? "").ToLowerInvariant();
                var automationId = (candidate.Current.AutomationId ?? "").ToLowerInvariant();
                var score = 0;
                if (name.Contains("address")) score += 500;
                if (name.Contains("search")) score += 250;
                if (name.Contains("url")) score += 250;
                if (automationId.Contains("address")) score += 500;
                if (score > bestScore)
                {
                    best = candidate;
                    bestScore = score;
                }
            }
            catch { /* Ignore stale browser chrome nodes. */ }
        }
        return best
            ?? throw new InvalidOperationException("The browser address bar was not exposed as an enabled visible control.");
    }

    private static int LabelScore(string? elementName, string? target)
    {
        var name = NormalizeText(elementName);
        if (string.IsNullOrWhiteSpace(name)) return int.MinValue;
        var words = TargetWords(target);
        if (words.Length == 0) return int.MinValue;
        var matched = words.Count(word => name.Contains(word, StringComparison.OrdinalIgnoreCase));
        if (matched == 0) return int.MinValue;
        var score = matched * 100;
        if (matched == words.Length) score += 250;
        if (NormalizeText(target).Contains(name, StringComparison.OrdinalIgnoreCase)) score += 300;
        return score;
    }

    private static AutomationElement FindTypingTarget(Process process, HostRequest request)
    {
        var automationId = GetOptionalString(request.Params, "automationId");
        var name = GetOptionalString(request.Params, "name");
        var controlType = GetOptionalString(request.Params, "controlType");
        if (!string.IsNullOrWhiteSpace(automationId) || !string.IsNullOrWhiteSpace(name) || !string.IsNullOrWhiteSpace(controlType))
        {
            var selected = FindElement(process, automationId, name, controlType);
            if (!IsUsableElement(selected))
                throw new InvalidOperationException("The requested text target is disabled or offscreen.");
            return selected;
        }

        var application = GetApplication(request);
        var root = RequireAutomationRoot(process, application);
        var isBrowser = BrowserApplications.Contains(application);
        var target = request.Target ?? "";
        var targetWantsSearch = target.Contains("search", StringComparison.OrdinalIgnoreCase)
            || target.Contains("address", StringComparison.OrdinalIgnoreCase);
        AutomationElement? best = null;
        var bestScore = int.MinValue;
        var descendants = root.FindAll(TreeScope.Descendants, Condition.TrueCondition);
        foreach (AutomationElement candidate in descendants.Cast<AutomationElement>())
        {
            try
            {
                var current = candidate.Current;
                var isEditableType = current.ControlType == ControlType.Edit
                    || (!isBrowser && current.ControlType == ControlType.Document);
                if (!isEditableType || !IsUsableElement(candidate)) continue;
                if (isBrowser && IsBrowserChromeEditor(candidate, process) && !targetWantsSearch) continue;
                var candidateName = current.Name ?? "";
                var lowerName = candidateName.ToLowerInvariant();
                var score = Math.Max(0, LabelScore(candidateName, target));
                if (current.ControlType == ControlType.Edit) score += 100;
                if (isBrowser && !targetWantsSearch
                    && (lowerName.Contains("search") || lowerName.Contains("address"))) score -= 800;
                var bounds = current.BoundingRectangle;
                score += (int)Math.Min(250, bounds.Width * bounds.Height / 500);
                if (score > bestScore)
                {
                    best = candidate;
                    bestScore = score;
                }
            }
            catch { /* Ignore stale nodes in dynamic browser pages. */ }
        }
        if (best is null)
            throw new InvalidOperationException("No enabled visible editable control matches the requested text target.");
        return best;
    }

    private static AutomationElement? FindClickTarget(Process process, HostRequest request, int requestedX, int requestedY)
    {
        if (string.IsNullOrWhiteSpace(request.Target)) return null;
        var root = TryAutomationRoot(process, GetApplication(request));
        if (root is null) return null;
        var isBrowser = BrowserApplications.Contains(GetApplication(request));
        var target = request.Target ?? "";
        var targetWantsBrowserChrome = target.Contains("address", StringComparison.OrdinalIgnoreCase)
            || target.Contains("browser toolbar", StringComparison.OrdinalIgnoreCase)
            || target.Contains("favorite", StringComparison.OrdinalIgnoreCase)
            || target.Contains("tab", StringComparison.OrdinalIgnoreCase)
            || target.Contains("reload", StringComparison.OrdinalIgnoreCase)
            || target.Contains("back button", StringComparison.OrdinalIgnoreCase);
        AutomationElement? best = null;
        var bestScore = double.MinValue;
        var descendants = root.FindAll(TreeScope.Descendants, Condition.TrueCondition);
        foreach (AutomationElement candidate in descendants.Cast<AutomationElement>())
        {
            try
            {
                var current = candidate.Current;
                if (current.IsOffscreen || !IsUsableElement(candidate)) continue;
                var type = current.ControlType;
                if (type == ControlType.Window || type == ControlType.Pane
                    || type == ControlType.Document || type == ControlType.Thumb)
                    continue;
                if (!InteractiveTypes.Contains(type) && !HasPattern(candidate, InvokePattern.Pattern))
                    continue;
                if (isBrowser && !targetWantsBrowserChrome && IsBrowserChromeControl(candidate, process)) continue;
                var labelScore = LabelScore(current.Name, request.Target);
                if (labelScore == int.MinValue) continue;
                var bounds = current.BoundingRectangle;
                if (bounds.Width <= 2 || bounds.Height <= 2) continue;
                var centerX = bounds.Left + bounds.Width / 2;
                var centerY = bounds.Top + bounds.Height / 2;
                var containsRequested = requestedX >= bounds.Left && requestedX < bounds.Right
                    && requestedY >= bounds.Top && requestedY < bounds.Bottom;
                var distance = Math.Sqrt(Math.Pow(centerX - requestedX, 2) + Math.Pow(centerY - requestedY, 2));
                var score = labelScore + (containsRequested ? 10_000 : 0) - distance / 8;
                if (score > bestScore)
                {
                    best = candidate;
                    bestScore = score;
                }
            }
            catch { /* Ignore stale nodes in dynamic browser pages. */ }
        }
        return best;
    }

    private static void RequireInsideWindow(int processId, int x, int y)
    {
        var process = Process.GetProcessById(processId);
        var handle = FindBestWindowHandle(process, null);
        if (!GetWindowRect(handle, out var rect))
            throw new InvalidOperationException("Could not read the target window bounds.");
        if (x < rect.Left || x >= rect.Right || y < rect.Top || y >= rect.Bottom)
            throw new InvalidOperationException("The recorded point is outside the target window; re-record this step.");
    }

    private static object Click(HostRequest request)
    {
        var process = ResolveProcess(request);
        FocusProcess(process, GetApplication(request));
        var markElement = TryResolveMark(request);
        int x;
        int y;
        AutomationElement? semanticTarget = markElement;
        if (markElement is not null)
        {
            (x, y) = CenterOf(markElement);
        }
        else if (TryPageOrWindowPoint(request, process, out x, out y))
        {
            // Window-relative 0–1 point from a screenshot / probe. Still retarget
            // onto a matching UIA control when one sits under the pixel.
            semanticTarget = FindClickTarget(process, request, x, y);
            var hit = HitTest(x, y);
            if (semanticTarget is null
                && hit is not null
                && hit.Current.ControlType != ControlType.Pane
                && hit.Current.ControlType != ControlType.Window
                && hit.Current.ControlType != ControlType.Document)
                semanticTarget = hit;
            RefuseUnverifiedBrowserCoordinate(request, semanticTarget);
        }
        else
        {
            x = GetInt(request.Params, "x");
            y = GetInt(request.Params, "y");
            RequireInsideWindow(process.Id, x, y);
            semanticTarget = FindClickTarget(process, request, x, y);
            RefuseUnverifiedBrowserCoordinate(request, semanticTarget);
        }
        string? targetName = null;
        string? controlType = null;
        if (semanticTarget is not null)
        {
            RefuseDestructiveControl(semanticTarget);
            var current = semanticTarget.Current;
            if (!current.IsEnabled)
                throw new InvalidOperationException($"The requested {request.Target} control is disabled; its prerequisite state has not been met.");
            targetName = current.Name;
            controlType = current.ControlType?.ProgrammaticName;
            (x, y) = CenterOf(semanticTarget);
        }
        else
        {
            var hit = HitTest(x, y);
            RefuseDestructiveControl(hit);
            try
            {
                targetName = hit?.Current.Name;
                controlType = hit?.Current.ControlType?.ProgrammaticName;
            }
            catch { /* The hit-test node can vanish before the click. */ }
        }
        RequireInsideWindow(process.Id, x, y);
        HumanLeftClick(process.Id, x, y);
        return new
        {
            clicked = true,
            how = "humanClick",
            x,
            y,
            mark = GetOptionalString(request.Params, "mark"),
            targetName,
            controlType
        };
    }

    private static object TypeText(HostRequest request, string text)
    {
        if (text.Contains("ΓÇ", StringComparison.Ordinal)
            || text.Contains("≡ƒ", StringComparison.Ordinal)
            || text.Contains("Ã", StringComparison.Ordinal)
            || text.Contains("â€", StringComparison.Ordinal))
            throw new InvalidOperationException("The requested text contains likely UTF-8 mojibake. Alfred will not submit corrupted content; the planner must regenerate the original Unicode or plain-ASCII text.");
        var process = ResolveProcess(request);
        FocusProcess(process, GetApplication(request));
        var target = TryResolveMark(request);
        var usedPagePoint = false;
        if (target is null && TryPageOrWindowPoint(request, process, out var pageX, out var pageY))
        {
            usedPagePoint = true;
            HumanLeftClick(process.Id, pageX, pageY);
            Thread.Sleep(80);
            target = AutomationElement.FocusedElement;
            if (target is null || !ElementBelongsToProcess(target, process, GetApplication(request)))
                target = HitTest(pageX, pageY);
        }
        target ??= FindTypingTarget(process, request);
        var application = GetApplication(request);
        if (!IsWritableElement(target) && !usedPagePoint)
        {
            try { target.SetFocus(); } catch { /* Search buttons often ignore SetFocus. */ }
            var hint = target.Current.BoundingRectangle;
            if (hint.Width > 2 && hint.Height > 2)
                HumanLeftClick(process.Id, (int)Math.Round(hint.Left + hint.Width / 2), (int)Math.Round(hint.Top + hint.Height / 2));
            Thread.Sleep(180);
            target = FindWritableSuccessor(process, application, target)
                ?? throw new InvalidOperationException("typeText requires an editable control; the supplied mark is not writable.");
        }
        RefusePasswordElement(target);
        var type = target.Current.ControlType;
        try { target.SetFocus(); } catch { /* Canvas / custom editors may reject SetFocus. */ }
        var bounds = target.Current.BoundingRectangle;
        if (bounds.Width > 2 && bounds.Height > 2)
            HumanLeftClick(process.Id, (int)Math.Round(bounds.Left + bounds.Width / 2), (int)Math.Round(bounds.Top + bounds.Height / 2));
        Thread.Sleep(150);
        var focused = AutomationElement.FocusedElement;
        // Store/WinUI search often hands focus to a child edit or helper process.
        if (focused is not null
            && IsWritableElement(focused)
            && KeyboardFocusAccepted(target, focused, process, application)
            && !SameVisualControl(target, focused))
        {
            target = focused;
            RefusePasswordElement(target);
            type = target.Current.ControlType;
            bounds = target.Current.BoundingRectangle;
        }
        if (BrowserApplications.Contains(application)
            && focused is not null
            && IsBrowserChromeEditor(focused, process)
            && !(request.Target ?? "").Contains("address", StringComparison.OrdinalIgnoreCase))
            throw new InvalidOperationException("Refusing to type page content into the browser address bar.");

        var before = ReadTargetSubtreeText(target);
        if (TextAppearsExactlyOnce(before, text))
        {
            return new
            {
                typed = false,
                alreadyPresent = true,
                verified = true,
                how = "alreadyPresent",
                characters = text.Length,
                observedText = Truncate(before, 500),
                targetName = target.Current.Name,
                controlType = target.Current.ControlType?.ProgrammaticName,
                bounds = JsonBounds(bounds)
            };
        }

        var usedDirectValue = false;
        var preferKeyboard = usedPagePoint
            || type == ControlType.Document
            || GetOptionalString(request.Params, "input") is "human";
        if (!preferKeyboard
            && target.TryGetCurrentPattern(ValuePattern.Pattern, out var valuePattern)
            && !((ValuePattern)valuePattern).Current.IsReadOnly)
        {
            ((ValuePattern)valuePattern).SetValue(text);
            usedDirectValue = true;
            Thread.Sleep(150);
            if (!TextAppearsExactlyOnce(ReadTargetSubtreeText(target), text))
            {
                usedDirectValue = false;
                preferKeyboard = true;
            }
        }
        if (!usedDirectValue)
        {
            if (!KeyboardFocusAccepted(target, AutomationElement.FocusedElement, process, application))
            {
                try { target.SetFocus(); } catch { /* Retry once after the click settles. */ }
                Thread.Sleep(120);
            }
            focused = AutomationElement.FocusedElement;
            if (!KeyboardFocusAccepted(target, focused, process, application))
                throw new InvalidOperationException(
                    $"The intended text target did not receive keyboard focus (focused={DescribeFocus(focused)}).");
            // Replacement semantics make retries safe. A previous attempt may
            // have changed the control even if readback timed out; selecting the
            // focused editor prevents appending a second/interleaved version.
            SelectAllInFocusedControl();
            Thread.Sleep(50);
            HumanTypeCharacters(text, process.Id);
        }
        Thread.Sleep(150);
        var observed = ReadTargetSubtreeText(target);
        var verified = TextAppearsExactlyOnce(observed, text);
        if (!verified && !usedPagePoint)
            throw new InvalidOperationException("The target was updated, but it does not contain exactly one verified copy of the requested text. Alfred will not append or submit it.");
        return new
        {
            typed = true,
            verified,
            usedDirectValue,
            how = usedDirectValue ? "setValue" : "humanType",
            characters = text.Length,
            observedText = Truncate(observed, 500),
            targetName = target.Current.Name,
            controlType = target.Current.ControlType?.ProgrammaticName,
            bounds = JsonBounds(bounds)
        };
    }

    private static object PressKey(HostRequest request, int virtualKey)
    {
        if (virtualKey == VK_DELETE)
            throw new UnauthorizedAccessException("The Delete key is blocked by Alfred's deletion policy.");
        if (!AllowedVirtualKeys.Contains(virtualKey))
            throw new InvalidOperationException($"Virtual key 0x{virtualKey:X2} is not in Alfred's allowed key set.");
        FocusProcess(ResolveProcess(request), GetApplication(request));
        HumanPressVirtualKey((ushort)virtualKey);
        return new { pressed = true, how = "humanKey", virtualKey };
    }

    private static object PressShortcut(HostRequest request, string keys)
    {
        if (!AllowedShortcuts.TryGetValue(keys.Trim(), out var virtualKey))
            throw new InvalidOperationException($"Shortcut {keys} is not in Alfred's allowed shortcut set.");
        FocusProcess(ResolveProcess(request), GetApplication(request));
        HumanPressShortcut(virtualKey);
        return new { pressed = true, how = "humanShortcut", keys = keys.ToUpperInvariant() };
    }

    private static int GetInt(JsonElement? value, string property) =>
        value?.GetProperty(property).GetInt32() ?? throw new InvalidOperationException($"Missing {property}.");
    private static int? GetOptionalInt(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) && item.TryGetInt32(out var result) ? result : null;
    private static double? GetOptionalDouble(JsonElement? value, string property)
    {
        if (!value.HasValue || !value.Value.TryGetProperty(property, out var item)) return null;
        if (item.ValueKind == JsonValueKind.Number && item.TryGetDouble(out var number)) return number;
        if (item.ValueKind == JsonValueKind.String
            && double.TryParse(item.GetString(), System.Globalization.NumberStyles.Float,
                System.Globalization.CultureInfo.InvariantCulture, out var parsed))
            return parsed;
        return null;
    }
    private static string GetString(JsonElement? value, string property) =>
        value?.GetProperty(property).GetString() ?? throw new InvalidOperationException($"Missing {property}.");
    private static string? GetOptionalString(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) ? item.GetString() : null;
    private static bool IsPasswordElement(AutomationElement element)
    {
        try { return element.Current.IsPassword; }
        catch { return false; }
    }

    private static void RefusePasswordElement(AutomationElement element)
    {
        if (IsPasswordElement(element))
            throw new UnauthorizedAccessException("Alfred will not type into a password field.");
    }

    private static bool IsWritableElement(AutomationElement element)
    {
        try
        {
            if (IsPasswordElement(element)) return false;
            var type = element.Current.ControlType;
            var hasWritableValue = element.TryGetCurrentPattern(ValuePattern.Pattern, out var pattern)
                && pattern is ValuePattern value
                && !value.Current.IsReadOnly;
            if (type == ControlType.Custom) return hasWritableValue;
            if (type == ControlType.Edit || type == ControlType.Document)
            {
                if (pattern is ValuePattern readOnly && readOnly.Current.IsReadOnly) return false;
                return true;
            }
            return hasWritableValue;
        }
        catch { return false; }
    }

    private static bool SameVisualControl(AutomationElement left, AutomationElement right)
    {
        try
        {
            var a = left.Current;
            var b = right.Current;
            if ((a.ControlType?.ProgrammaticName ?? "") != (b.ControlType?.ProgrammaticName ?? ""))
                return false;
            if ((a.Name ?? "") != (b.Name ?? "")) return false;
            if (!string.IsNullOrWhiteSpace(a.AutomationId)
                && !a.AutomationId.Equals(b.AutomationId ?? "", StringComparison.Ordinal))
                return false;
            var ab = a.BoundingRectangle;
            var bb = b.BoundingRectangle;
            return Math.Abs(ab.X - bb.X) < 12
                && Math.Abs(ab.Y - bb.Y) < 12
                && Math.Abs(ab.Width - bb.Width) < 24
                && Math.Abs(ab.Height - bb.Height) < 24;
        }
        catch { return false; }
    }

    private static bool KeyboardFocusAccepted(
        AutomationElement target,
        AutomationElement? focused,
        Process process,
        string? application)
    {
        if (focused is null) return false;
        if (SameVisualControl(target, focused)) return true;
        try
        {
            var box = target.Current.BoundingRectangle;
            var focusBox = focused.Current.BoundingRectangle;
            if (box.Width <= 0 || box.Height <= 0) return false;
            var centerX = focusBox.X + focusBox.Width / 2;
            var centerY = focusBox.Y + focusBox.Height / 2;
            if (centerX >= box.X && centerX <= box.X + box.Width
                && centerY >= box.Y && centerY <= box.Y + box.Height)
                return true;
            var distance = Math.Sqrt(
                Math.Pow(centerX - (box.X + box.Width / 2), 2)
                + Math.Pow(centerY - (box.Y + box.Height / 2), 2));
            return distance <= 120 && ElementBelongsToProcess(focused, process, application);
        }
        catch { return false; }
    }

    private static AutomationElement? FindWritableSuccessor(
        Process process,
        string application,
        AutomationElement origin)
    {
        try
        {
            var focused = AutomationElement.FocusedElement;
            if (focused is not null
                && IsWritableElement(focused)
                && KeyboardFocusAccepted(origin, focused, process, application))
                return focused;
        }
        catch { /* WinUI may replace the Search button before focus settles. */ }

        var root = TryAutomationRoot(process, application);
        if (root is null) return null;
        string originName;
        System.Windows.Rect originBox;
        try
        {
            originName = origin.Current.Name ?? "";
            originBox = origin.Current.BoundingRectangle;
        }
        catch { return null; }

        AutomationElement? best = null;
        var bestScore = double.MinValue;
        foreach (AutomationElement candidate in root.FindAll(TreeScope.Descendants, Condition.TrueCondition).Cast<AutomationElement>())
        {
            try
            {
                if (!IsUsableElement(candidate) || !IsWritableElement(candidate)) continue;
                var current = candidate.Current;
                if (current.IsOffscreen) continue;
                var box = current.BoundingRectangle;
                if (box.Width <= 2 || box.Height <= 2) continue;
                var score = 0.0;
                if (!string.IsNullOrWhiteSpace(originName)
                    && (current.Name ?? "").Equals(originName, StringComparison.OrdinalIgnoreCase))
                    score += 500;
                var distance = Math.Sqrt(
                    Math.Pow((box.X + box.Width / 2) - (originBox.X + originBox.Width / 2), 2)
                    + Math.Pow((box.Y + box.Height / 2) - (originBox.Y + originBox.Height / 2), 2));
                if (distance > 400) continue;
                if (distance > 240 && score < 500) continue;
                score -= distance / 4;
                if (score > bestScore)
                {
                    best = candidate;
                    bestScore = score;
                }
            }
            catch { /* Skip stale nodes while Store redraws search. */ }
        }
        return bestScore >= -60 ? best : null;
    }

    private static string DescribeFocus(AutomationElement? focused)
    {
        if (focused is null) return "none";
        try
        {
            var current = focused.Current;
            var processName = "";
            try
            {
                using var owner = Process.GetProcessById(current.ProcessId);
                processName = owner.ProcessName ?? "";
            }
            catch { /* Access-denied helper processes still report a pid. */ }
            return $"{processName}#{current.ProcessId} {current.ControlType?.ProgrammaticName} '{Truncate(current.Name ?? "", 80)}'";
        }
        catch { return "unavailable"; }
    }

    private static string Truncate(string value, int max) => value.Length <= max ? value : value[..max];
    private static void Reply(object value) { Console.Out.WriteLine(JsonSerializer.Serialize(value, Json)); Console.Out.Flush(); }

    private static INPUT MouseInput(uint flags) => new() { type = INPUT_MOUSE, U = new InputUnion { mi = new MOUSEINPUT { dwFlags = flags } } };
    private static INPUT KeyboardInput(char value, bool up) => new() { type = INPUT_KEYBOARD, U = new InputUnion { ki = new KEYBDINPUT { wScan = value, dwFlags = KEYEVENTF_UNICODE | (up ? KEYEVENTF_KEYUP : 0) } } };
    private static INPUT VirtualKeyInput(ushort value, bool up) => new() { type = INPUT_KEYBOARD, U = new InputUnion { ki = new KEYBDINPUT { wVk = value, dwFlags = up ? KEYEVENTF_KEYUP : 0 } } };
    private static void Send(INPUT[] inputs)
    {
        if (SendInput((uint)inputs.Length, inputs, Marshal.SizeOf<INPUT>()) != inputs.Length)
            throw new InvalidOperationException("Windows rejected the input event.");
    }

    private const uint INPUT_MOUSE = 0, INPUT_KEYBOARD = 1;
    private const uint MOUSEEVENTF_MOVE = 0x0001;
    private const uint MOUSEEVENTF_LEFTDOWN = 0x0002, MOUSEEVENTF_LEFTUP = 0x0004;
    private const uint MOUSEEVENTF_RIGHTDOWN = 0x0008, MOUSEEVENTF_RIGHTUP = 0x0010;
    private const uint MOUSEEVENTF_WHEEL = 0x0800;
    private const uint MOUSEEVENTF_HWHEEL = 0x1000;
    private const uint KEYEVENTF_KEYUP = 0x0002, KEYEVENTF_UNICODE = 0x0004;
    [StructLayout(LayoutKind.Sequential)] private struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] private struct INPUT { public uint type; public InputUnion U; }
    [StructLayout(LayoutKind.Explicit)] private struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
    [StructLayout(LayoutKind.Sequential)] private struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] private struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr dwExtraInfo; }
    [DllImport("user32.dll")] private static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] private static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll", SetLastError = true)] private static extern uint SendInput(uint count, INPUT[] inputs, int size);
    [DllImport("user32.dll")] private static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] private static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] private static extern IntPtr GetAncestor(IntPtr hWnd, uint flags);
    private const uint GA_ROOT = 2;
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
    [DllImport("user32.dll")] private static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] private static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] private static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] private static extern bool SetWindowPos(IntPtr hWnd, IntPtr insertAfter, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] private static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool attach);
    [DllImport("kernel32.dll")] private static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] private static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
    [DllImport("user32.dll")] private static extern uint GetDpiForWindow(IntPtr hWnd);
    [DllImport("user32.dll")] private static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    private static readonly IntPtr DpiAwarenessContextPerMonitorV2 = unchecked((IntPtr)(-4));
}
