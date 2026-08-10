using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Runtime.InteropServices;
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
    private static readonly string[] DestructiveWords = ["delete", "remove", "erase", "trash", "purge", "wipe", "shred", "overwrite", "empty recycle"];

    // Only these applications may be launched by name; everything else is refused.
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

    private const int SW_RESTORE = 9;
    private const uint PW_RENDERFULLCONTENT = 0x00000002;

    [STAThread]
    private static async Task Main(string[] args)
    {
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
        "resolveApplication" => ResolveApplication(GetString(request.Params, "name")),
        "launchApplication" => LaunchApplication(request),
        "focusApplication" => FocusApplication(request),
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
            .Select(item => new { processId = item.id, name = item.name, title = item.title })
            .ToArray();
    }

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
        if (!LaunchTargets.TryGetValue(application, out var executable))
            throw new UnauthorizedAccessException($"Alfred is not allowed to launch {application}.");
        var process = Process.Start(new ProcessStartInfo(executable) { UseShellExecute = true })
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
                try { AutomationElement.FromHandle(handle)?.SetFocus(); } catch { }
                SetForegroundWindow(handle);
                BringWindowToTop(handle);
                lastAttempt = DateTime.UtcNow;
            }
            Thread.Sleep(100);
        }
        throw new InvalidOperationException("Could not bring the target window to the foreground.");
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
        return Snapshot(root, 0);
    }

    private static object Snapshot(AutomationElement element, int depth)
    {
        var properties = element.Current;
        var children = depth >= 5 ? [] : element.FindAll(TreeScope.Children, Condition.TrueCondition)
            .Cast<AutomationElement>().Take(250).Select(item => Snapshot(item, depth + 1)).ToArray();
        var bounds = properties.BoundingRectangle;
        return new
        {
            name = properties.Name,
            automationId = properties.AutomationId,
            controlType = properties.ControlType?.ProgrammaticName,
            enabled = properties.IsEnabled,
            offscreen = properties.IsOffscreen,
            bounds = new { x = bounds.X, y = bounds.Y, width = bounds.Width, height = bounds.Height },
            children
        };
    }

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
                bounds = new { x = bounds.X, y = bounds.Y, width = bounds.Width, height = bounds.Height }
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
        return new { set = true, characters = value.Length };
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
        if (!string.IsNullOrWhiteSpace(request.Application) || GetOptionalInt(request.Params, "processId").HasValue)
        {
            var process = ResolveProcess(request);
            FocusProcess(process);
            RequireInsideWindow(process.Id, x, y);
        }
        SetCursorPos(x, y);
        Send([MouseInput(MOUSEEVENTF_LEFTDOWN), MouseInput(MOUSEEVENTF_LEFTUP)]);
        return new { clicked = true, x, y };
    }

    private static object TypeText(HostRequest request, string text)
    {
        FocusProcess(ResolveProcess(request));
        foreach (var character in text)
            Send([KeyboardInput(character, false), KeyboardInput(character, true)]);
        return new { typed = true, characters = text.Length };
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

    private static int GetInt(JsonElement? value, string property) =>
        value?.GetProperty(property).GetInt32() ?? throw new InvalidOperationException($"Missing {property}.");
    private static int? GetOptionalInt(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) && item.TryGetInt32(out var result) ? result : null;
    private static string GetString(JsonElement? value, string property) =>
        value?.GetProperty(property).GetString() ?? throw new InvalidOperationException($"Missing {property}.");
    private static string? GetOptionalString(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) ? item.GetString() : null;
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
    [DllImport("user32.dll")] private static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
}
