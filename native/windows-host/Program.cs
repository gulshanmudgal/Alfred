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

internal static class Program
{
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);
    private static readonly string ExpectedToken = Environment.GetEnvironmentVariable("ALFRED_CAPABILITY_TOKEN") ?? "";
    private static readonly string[] DestructiveWords = ["delete", "remove", "erase", "trash", "purge", "wipe", "shred", "overwrite", "uninstall", "empty recycle"];

    // Stable Windows inbox/browser aliases. Other applications are launchable
    // only when an exact Start-menu shortcut is installed; the planner can never
    // supply an executable path or arbitrary command line.
    private static readonly IReadOnlyDictionary<string, string> LaunchTargets = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase)
    {
        ["Notepad"] = "notepad.exe",
        ["Calculator"] = "calc.exe",
        ["Paint"] = "mspaint.exe",
        ["File Explorer"] = "explorer.exe",
        ["Microsoft Edge"] = "msedge.exe",
        ["Google Chrome"] = "chrome.exe",
        ["Brave"] = "brave.exe"
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

    private const int SW_RESTORE = 9;
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
        var text = $"{request.Method} {request.Intent} {request.Target} {request.Params}".ToLowerInvariant();
        if (DestructiveWords.Any(text.Contains))
            throw new UnauthorizedAccessException("Destructive actions are blocked by the Windows host.");
    }

    private static object Dispatch(HostRequest request) => request.Method switch
    {
        "health" => new { host = "windows", version = "0.1.3", processId = Environment.ProcessId },
        "listApplications" => ListApplications(),
        "listInstalledApplications" => ListInstalledApplications(),
        "resolveApplication" => ResolveApplication(GetString(request.Params, "name")),
        "launchApplication" => LaunchApplication(request),
        "focusApplication" => FocusApplication(request),
        "navigateApplication" => NavigateApplication(request),
        "activate" => Activate(ResolveProcess(request).Id),
        "observeWindow" => ObserveWindow(ResolveProcess(request).Id),
        "captureWindow" => CaptureWindow(ResolveProcess(request).Id),
        "findElement" => FindElementInfo(request),
        "getValue" => GetElementValue(request),
        "invokeElement" => InvokeElement(request),
        "setValue" => SetElementValue(request),
        "click" => Click(request, GetInt(request.Params, "x"), GetInt(request.Params, "y")),
        "typeText" => TypeText(request, GetString(request.Params, "text")),
        "key" => PressKey(request, GetInt(request.Params, "virtualKey")),
        "shortcut" => PressShortcut(request, GetString(request.Params, "keys")),
        _ => throw new InvalidOperationException($"Unsupported host method: {request.Method}")
    };

    private static object ListApplications()
    {
        var items = new List<(int id, string name, string title)>();
        foreach (var process in Process.GetProcesses())
        {
            try
            {
                if (process.MainWindowHandle == IntPtr.Zero) continue;
                items.Add((process.Id, process.ProcessName ?? "", process.MainWindowTitle ?? ""));
            }
            catch { /* The process exited or denies access; skip it. */ }
        }
        return items.OrderBy(item => item.name)
            .Take(200)
            .Select(item => new { processId = item.id, name = item.name, title = Truncate(item.title, 160) })
            .ToArray();
    }

    private static IEnumerable<string> StartMenuRoots()
    {
        var common = Environment.GetFolderPath(Environment.SpecialFolder.CommonStartMenu);
        var user = Environment.GetFolderPath(Environment.SpecialFolder.StartMenu);
        if (!string.IsNullOrWhiteSpace(common)) yield return Path.Combine(common, "Programs");
        if (!string.IsNullOrWhiteSpace(user)) yield return Path.Combine(user, "Programs");
    }

    private static IReadOnlyList<(string Name, string Path)> StartMenuApplications()
    {
        var applications = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var root in StartMenuRoots())
        {
            if (!Directory.Exists(root)) continue;
            try
            {
                foreach (var path in Directory.EnumerateFiles(root, "*.lnk", SearchOption.AllDirectories))
                {
                    var name = Path.GetFileNameWithoutExtension(path).Trim();
                    if (!string.IsNullOrWhiteSpace(name)) applications.TryAdd(name, path);
                }
            }
            catch { /* A vendor-owned Start-menu folder may deny traversal. */ }
        }
        return applications.OrderBy(item => item.Key)
            .Select(item => (item.Key, item.Value)).ToArray();
    }

    private static object ListInstalledApplications() => StartMenuApplications()
        .Select(item => item.Name)
        .Concat(LaunchTargets.Keys)
        .Distinct(StringComparer.OrdinalIgnoreCase)
        .OrderBy(name => name)
        .Take(300)
        .Select(name => new { name })
        .ToArray();

    private static string? FindStartMenuShortcut(string application) => StartMenuApplications()
        .FirstOrDefault(item => item.Name.Equals(application, StringComparison.OrdinalIgnoreCase))
        .Path;

    // Token-scored name-to-window resolution used by Alfred Core for preflight and
    // state conditions. LaunchTargets names resolve through their executable name.
    private static object ResolveApplication(string name)
    {
        var tokens = name.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
            .Where(token => token.Length >= 3)
            .Select(token => token.ToLowerInvariant())
            .ToArray();
        if (tokens.Length == 0)
            throw new InvalidOperationException("An application name is required to resolve a window.");
        var lowered = name.ToLowerInvariant();
        (int id, string name, string title, int score)? best = null;
        foreach (var process in Process.GetProcesses())
        {
            try
            {
                if (process.MainWindowHandle == IntPtr.Zero) continue;
                var processName = (process.ProcessName ?? "").ToLowerInvariant();
                var title = (process.MainWindowTitle ?? "").ToLowerInvariant();
                var score = processName == lowered ? 100 : 0;
                foreach (var token in tokens)
                {
                    if (processName.Contains(token)) score += 10;
                    if (title.Contains(token)) score += 5;
                }
                if (score == 0) continue;
                if (best is null || score > best.Value.score)
                    best = (process.Id, process.ProcessName ?? "", process.MainWindowTitle ?? "", score);
            }
            catch { /* The process exited or denies access; skip it. */ }
        }
        if (best is null)
            throw new InvalidOperationException($"No running application window matches \"{name}\".");
        return new { processId = best.Value.id, name = best.Value.name, title = best.Value.title, matched = name };
    }

    private static object LaunchApplication(HostRequest request)
    {
        var application = GetApplication(request);
        var target = LaunchTargets.TryGetValue(application, out var executable)
            ? executable
            : FindStartMenuShortcut(application);
        if (string.IsNullOrWhiteSpace(target))
            throw new UnauthorizedAccessException($"{application} is not an exact installed Start-menu application.");
        var existing = FindApplicationProcess(application);
        if (existing is not null)
        {
            FocusProcess(existing);
            return new { launched = false, alreadyRunning = true, application, processId = existing.Id, title = existing.MainWindowTitle };
        }
        var process = Process.Start(new ProcessStartInfo(target) { UseShellExecute = true })
            ?? throw new InvalidOperationException($"Windows could not launch {application}.");
        try { process.WaitForInputIdle(5000); } catch { }
        for (var attempt = 0; attempt < 40 && process.MainWindowHandle == IntPtr.Zero; attempt++)
        {
            Thread.Sleep(100);
            process.Refresh();
        }
        if (process.MainWindowHandle == IntPtr.Zero)
            process = FindApplicationProcess(application)
                ?? throw new InvalidOperationException($"{application} launched without an accessible window.");
        FocusProcess(process);
        return new { launched = true, application, processId = process.Id, title = process.MainWindowTitle };
    }

    private static object FocusApplication(HostRequest request)
    {
        var process = ResolveProcess(request);
        FocusProcess(process);
        return new { focused = true, application = GetApplication(request), processId = process.Id, title = process.MainWindowTitle };
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
        FocusProcess(process);
        var addressBar = FindBrowserAddressBar(process);
        addressBar.SetFocus();
        Thread.Sleep(100);
        var focused = AutomationElement.FocusedElement;
        if (focused is null || focused.Current.ProcessId != process.Id || !IsBrowserChromeEditor(focused, process))
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
            foreach (var character in url)
                Send([KeyboardInput(character, false), KeyboardInput(character, true)]);
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
        return new { activated = true, processId, title = process.MainWindowTitle };
    }

    // Brings the target window to the foreground and proves it stayed there before
    // any input is sent. UIA SetFocus is attempted first; SetForegroundWindow is
    // retried while Windows settles.
    private static void FocusProcess(Process process)
    {
        process.Refresh();
        var handle = process.MainWindowHandle;
        if (handle == IntPtr.Zero)
            throw new InvalidOperationException("The application does not have an accessible window.");
        if (IsIconic(handle)) ShowWindow(handle, SW_RESTORE);
        var deadline = DateTime.UtcNow.AddSeconds(5);
        var lastAttempt = DateTime.MinValue;
        while (DateTime.UtcNow < deadline)
        {
            var foreground = GetForegroundWindow();
            GetWindowThreadProcessId(foreground, out var foregroundPid);
            if (foreground == handle || foregroundPid == (uint)process.Id)
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
        throw new InvalidOperationException("Could not bring the target window to the foreground.");
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
        }
        finally
        {
            if (attachedTarget) AttachThreadInput(currentThread, targetThread, false);
            if (attachedForeground) AttachThreadInput(currentThread, foregroundThread, false);
        }
    }

    private static Process ResolveProcess(HostRequest request)
    {
        var processId = GetOptionalInt(request.Params, "processId");
        if (processId.HasValue)
            return Process.GetProcessById(processId.Value);
        return FindApplicationProcess(GetApplication(request))
            ?? throw new InvalidOperationException($"{GetApplication(request)} is not open.");
    }

    private static Process? FindApplicationProcess(string application)
    {
        LaunchTargets.TryGetValue(application, out var executable);
        var expectedName = Path.GetFileNameWithoutExtension(executable ?? application);
        return Process.GetProcesses()
            .Where(process => process.MainWindowHandle != IntPtr.Zero)
            .OrderByDescending(process => process.Id)
            .FirstOrDefault(process =>
            {
                try
                {
                    return process.ProcessName.Equals(expectedName, StringComparison.OrdinalIgnoreCase)
                        || (process.MainWindowTitle ?? "").Contains(application, StringComparison.OrdinalIgnoreCase);
                }
                catch { return false; }
            });
    }

    private static string GetApplication(HostRequest request) =>
        string.IsNullOrWhiteSpace(request.Application)
            ? GetOptionalString(request.Params, "application")
                ?? throw new InvalidOperationException("Missing application.")
            : request.Application.Trim();

    private static object ObserveWindow(int processId)
    {
        var process = Process.GetProcessById(processId);
        var root = AutomationElement.FromHandle(process.MainWindowHandle)
            ?? throw new InvalidOperationException("The application window is unavailable.");
        var remainingNodes = MaxSnapshotNodes;
        return Snapshot(root, 0, ref remainingNodes);
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

    private static object CaptureWindow(int processId)
    {
        var process = Process.GetProcessById(processId);
        var handle = process.MainWindowHandle;
        if (handle == IntPtr.Zero)
            throw new InvalidOperationException("The application window is unavailable.");
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
        using var output = new MemoryStream();
        bitmap.Save(output, ImageFormat.Png);
        return new { mimeType = "image/png", width, height, base64 = Convert.ToBase64String(output.ToArray()) };
    }

    private static AutomationElement FindElement(Process process, string? automationId, string? name, string? controlType)
    {
        var root = AutomationElement.FromHandle(process.MainWindowHandle)
            ?? throw new InvalidOperationException("The application window is unavailable.");
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
            var element = FindElement(process,
                GetOptionalString(request.Params, "automationId"),
                GetOptionalString(request.Params, "name"),
                GetOptionalString(request.Params, "controlType"));
            var bounds = element.Current.BoundingRectangle;
            return new
            {
                found = true,
                name = element.Current.Name,
                automationId = element.Current.AutomationId,
                controlType = element.Current.ControlType?.ProgrammaticName,
                bounds = JsonBounds(bounds)
            };
        }
        catch
        {
            return new { found = false, name = "", automationId = "", controlType = "" };
        }
    }

    private static object GetElementValue(HostRequest request)
    {
        var process = ResolveProcess(request);
        var element = FindElement(process,
            GetOptionalString(request.Params, "automationId"),
            GetOptionalString(request.Params, "name"),
            GetOptionalString(request.Params, "controlType"));
        if (element.TryGetCurrentPattern(ValuePattern.Pattern, out var pattern))
            return new { value = ((ValuePattern)pattern).Current.Value };
        var name = element.Current.Name;
        if (!string.IsNullOrEmpty(name)) return new { value = name };
        throw new InvalidOperationException("The UI element exposes no readable value.");
    }

    private static object InvokeElement(HostRequest request)
    {
        var process = ResolveProcess(request);
        var element = FindElement(process,
            GetOptionalString(request.Params, "automationId"),
            GetOptionalString(request.Params, "name"),
            GetOptionalString(request.Params, "controlType"));
        if (!element.TryGetCurrentPattern(InvokePattern.Pattern, out var pattern))
            throw new InvalidOperationException("The UI element is not invokable.");
        ((InvokePattern)pattern).Invoke();
        return new { invoked = true };
    }

    private static object SetElementValue(HostRequest request)
    {
        var value = GetString(request.Params, "value");
        var process = ResolveProcess(request);
        var element = FindElement(process,
            GetOptionalString(request.Params, "automationId"),
            GetOptionalString(request.Params, "name"),
            GetOptionalString(request.Params, "controlType"));
        if (!element.TryGetCurrentPattern(ValuePattern.Pattern, out var pattern))
            throw new InvalidOperationException("The UI element does not accept direct values; use activate plus typeText.");
        ((ValuePattern)pattern).SetValue(value);
        Thread.Sleep(100);
        var observed = ReadElementText(element);
        if (!TextAppearsExactlyOnce(observed, value))
            throw new InvalidOperationException("The value action returned, but the requested text was not present in the target control.");
        return new
        {
            set = true,
            verified = true,
            characters = value.Length,
            observedText = Truncate(observed, 500),
            targetName = element.Current.Name,
            controlType = element.Current.ControlType?.ProgrammaticName
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
        return string.Join(" ", values);
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
            if (!GetWindowRect(process.MainWindowHandle, out var window)) return false;
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
            if (!GetWindowRect(process.MainWindowHandle, out var window)) return false;
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
        var root = AutomationElement.FromHandle(process.MainWindowHandle)
            ?? throw new InvalidOperationException("The browser window is unavailable.");
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

        var root = AutomationElement.FromHandle(process.MainWindowHandle)
            ?? throw new InvalidOperationException("The application window is unavailable.");
        var application = GetApplication(request);
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
        var root = AutomationElement.FromHandle(process.MainWindowHandle);
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
                if (current.IsOffscreen) continue;
                if (current.ControlType != ControlType.Button
                    && current.ControlType != ControlType.Hyperlink
                    && current.ControlType != ControlType.MenuItem
                    && current.ControlType != ControlType.ListItem) continue;
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
        if (!GetWindowRect(process.MainWindowHandle, out var rect))
            throw new InvalidOperationException("Could not read the target window bounds.");
        if (x < rect.Left || x >= rect.Right || y < rect.Top || y >= rect.Bottom)
            throw new InvalidOperationException("The recorded point is outside the target window; re-record this step.");
    }

    private static object Click(HostRequest request, int x, int y)
    {
        AutomationElement? semanticTarget = null;
        Process? process = null;
        if (!string.IsNullOrWhiteSpace(request.Application) || GetOptionalInt(request.Params, "processId").HasValue)
        {
            process = ResolveProcess(request);
            FocusProcess(process);
            RequireInsideWindow(process.Id, x, y);
            semanticTarget = FindClickTarget(process, request, x, y);
            if (BrowserApplications.Contains(GetApplication(request))
                && !string.IsNullOrWhiteSpace(request.Target)
                && semanticTarget is null)
                throw new InvalidOperationException($"No enabled visible browser page control matches '{request.Target}'. Alfred will not fall back to an unverified browser coordinate.");
            if (semanticTarget is not null)
            {
                var current = semanticTarget.Current;
                if (!current.IsEnabled)
                    throw new InvalidOperationException($"The requested {request.Target} control is disabled; its prerequisite state has not been met.");
                var bounds = current.BoundingRectangle;
                x = (int)Math.Round(bounds.Left + bounds.Width / 2);
                y = (int)Math.Round(bounds.Top + bounds.Height / 2);
                RequireInsideWindow(process.Id, x, y);
            }
        }
        SetCursorPos(x, y);
        Send([MouseInput(MOUSEEVENTF_LEFTDOWN), MouseInput(MOUSEEVENTF_LEFTUP)]);
        return new
        {
            clicked = true,
            x,
            y,
            targetName = semanticTarget?.Current.Name,
            controlType = semanticTarget?.Current.ControlType?.ProgrammaticName
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
        FocusProcess(process);
        var target = FindTypingTarget(process, request);
        target.SetFocus();
        var bounds = target.Current.BoundingRectangle;
        SetCursorPos(
            (int)Math.Round(bounds.Left + bounds.Width / 2),
            (int)Math.Round(bounds.Top + bounds.Height / 2));
        Send([MouseInput(MOUSEEVENTF_LEFTDOWN), MouseInput(MOUSEEVENTF_LEFTUP)]);
        Thread.Sleep(100);
        var focused = AutomationElement.FocusedElement;
        if (focused is null || focused.Current.ProcessId != process.Id)
            throw new InvalidOperationException("The intended text target did not receive keyboard focus.");
        if (BrowserApplications.Contains(GetApplication(request))
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
                characters = text.Length,
                observedText = Truncate(before, 500),
                targetName = target.Current.Name,
                controlType = target.Current.ControlType?.ProgrammaticName,
                bounds = JsonBounds(bounds)
            };
        }

        var usedDirectValue = false;
        if (target.TryGetCurrentPattern(ValuePattern.Pattern, out var valuePattern)
            && !((ValuePattern)valuePattern).Current.IsReadOnly)
        {
            ((ValuePattern)valuePattern).SetValue(text);
            usedDirectValue = true;
        }
        else
        {
            // Replacement semantics make retries safe. A previous attempt may
            // have changed the control even if readback timed out; selecting the
            // focused editor prevents appending a second/interleaved version.
            SelectAllInFocusedControl();
            Thread.Sleep(50);
            foreach (var character in text)
                Send([KeyboardInput(character, false), KeyboardInput(character, true)]);
        }
        Thread.Sleep(150);
        var observed = ReadTargetSubtreeText(target);
        if (!TextAppearsExactlyOnce(observed, text))
            throw new InvalidOperationException("The target was updated, but it does not contain exactly one verified copy of the requested text. Alfred will not append or submit it.");
        return new
        {
            typed = true,
            verified = true,
            usedDirectValue,
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
        FocusProcess(ResolveProcess(request));
        Send([VirtualKeyInput((ushort)virtualKey, false), VirtualKeyInput((ushort)virtualKey, true)]);
        return new { pressed = true, virtualKey };
    }

    private static object PressShortcut(HostRequest request, string keys)
    {
        if (!AllowedShortcuts.TryGetValue(keys.Trim(), out var virtualKey))
            throw new InvalidOperationException($"Shortcut {keys} is not in Alfred's allowed shortcut set.");
        FocusProcess(ResolveProcess(request));
        const ushort control = 0x11;
        Send([
            VirtualKeyInput(control, false),
            VirtualKeyInput(virtualKey, false),
            VirtualKeyInput(virtualKey, true),
            VirtualKeyInput(control, true)
        ]);
        return new { pressed = true, keys = keys.ToUpperInvariant() };
    }

    private static int GetInt(JsonElement? value, string property) =>
        value?.GetProperty(property).GetInt32() ?? throw new InvalidOperationException($"Missing {property}.");
    private static int? GetOptionalInt(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) && item.TryGetInt32(out var result) ? result : null;
    private static string GetString(JsonElement? value, string property) =>
        value?.GetProperty(property).GetString() ?? throw new InvalidOperationException($"Missing {property}.");
    private static string? GetOptionalString(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) ? item.GetString() : null;
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

    private const uint INPUT_MOUSE = 0, INPUT_KEYBOARD = 1, MOUSEEVENTF_LEFTDOWN = 0x0002, MOUSEEVENTF_LEFTUP = 0x0004;
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
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
    [DllImport("user32.dll")] private static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] private static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] private static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] private static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool attach);
    [DllImport("kernel32.dll")] private static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] private static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
}
