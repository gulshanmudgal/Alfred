using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Windows.Automation;

internal static partial class Program
{
    // Known process names for inbox/UWP apps whose executable is a stub or whose
    // window is hosted by ApplicationFrameHost. Planner-facing names stay human.
    private static readonly IReadOnlyDictionary<string, string[]> ProcessAliases =
        new Dictionary<string, string[]>(StringComparer.OrdinalIgnoreCase)
        {
            ["Notepad"] = ["notepad"],
            ["Calculator"] = ["CalculatorApp", "calc"],
            ["Paint"] = ["mspaint"],
            ["File Explorer"] = ["explorer"],
            ["Microsoft Edge"] = ["msedge"],
            ["Google Chrome"] = ["chrome"],
            ["Brave"] = ["brave"],
            ["Microsoft Store"] = ["WinStore.App", "WinStore.Mobile", "StoreExperienceHost"],
            ["Settings"] = ["SystemSettings"],
            ["Windows Terminal"] = ["WindowsTerminal", "wt"],
            ["Snipping Tool"] = ["SnippingTool", "ScreenClippingHost"],
            ["Photos"] = ["Photos", "Microsoft.Photos"]
        };

    private static readonly HashSet<string> BrowserProcessNames = new(StringComparer.OrdinalIgnoreCase)
    {
        "msedge", "chrome", "brave", "msedgewebview2"
    };

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

    // shell:AppsFolder is the complete Start-menu catalog, including Store/UWP
    // packages that never drop a classic .lnk under Programs.
    private static IReadOnlyList<(string Name, string Path)> AppsFolderApplications()
    {
        var applications = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        try
        {
            var shellType = Type.GetTypeFromProgID("Shell.Application");
            if (shellType is null) return [];
            dynamic shell = Activator.CreateInstance(shellType)!;
            dynamic? folder = shell.NameSpace("shell:AppsFolder");
            if (folder is null) return [];
            foreach (dynamic item in folder.Items())
            {
                try
                {
                    var name = ((string?)item.Name ?? "").Trim();
                    var path = ((string?)item.Path ?? "").Trim();
                    if (name.Length == 0 || path.Length == 0) continue;
                    if (name.Contains("uninstall", StringComparison.OrdinalIgnoreCase)) continue;
                    applications.TryAdd(name, "aumid:" + path);
                }
                catch { /* A single AppsFolder item can fail without poisoning the catalog. */ }
            }
        }
        catch { /* Shell.Application is unavailable in some constrained sessions. */ }
        return applications.Select(item => (item.Key, item.Value)).ToArray();
    }

    private static IReadOnlyList<(string Name, string Path)> AllInstalledApplications()
    {
        var applications = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var (name, path) in LaunchTargets)
            applications.TryAdd(name, path);
        foreach (var (name, path) in StartMenuApplications())
            applications.TryAdd(name, path);
        foreach (var (name, path) in AppsFolderApplications())
            applications.TryAdd(name, path);
        return applications.OrderBy(item => item.Key, StringComparer.OrdinalIgnoreCase)
            .Select(item => (item.Key, item.Value))
            .ToArray();
    }

    private static object ListInstalledApplications(HostRequest request)
    {
        var query = (GetOptionalString(request.Params, "query") ?? "").Trim();
        IEnumerable<string> names = AllInstalledApplications().Select(item => item.Name);
        if (query.Length > 0)
        {
            names = names
                .Select(name => (name, Score: ScoreInstalledName(query, name)))
                .Where(item => item.Score >= 200)
                .OrderByDescending(item => item.Score)
                .ThenBy(item => item.name.Length)
                .Select(item => item.name)
                .Take(40);
        }
        else
        {
            names = names.Take(400);
        }
        return names.Select(name => new { name }).ToArray();
    }

    internal static int ScoreInstalledName(string requested, string candidate)
    {
        var wanted = (requested ?? "").Trim();
        var name = (candidate ?? "").Trim();
        if (wanted.Length == 0 || name.Length == 0) return 0;
        if (wanted.Equals(name, StringComparison.OrdinalIgnoreCase)) return 1000;
        var wantedNorm = NormalizeText(wanted);
        var nameNorm = NormalizeText(name);
        if (wantedNorm.Length == 0 || nameNorm.Length == 0) return 0;
        if (wantedNorm == nameNorm) return 900;
        if (nameNorm.StartsWith(wantedNorm, StringComparison.Ordinal) || wantedNorm.StartsWith(nameNorm, StringComparison.Ordinal))
            return 700 + Math.Min(wantedNorm.Length, nameNorm.Length);
        if (nameNorm.Contains(wantedNorm, StringComparison.Ordinal)) return 500 + wantedNorm.Length;
        if (wantedNorm.Contains(nameNorm, StringComparison.Ordinal) && nameNorm.Length >= 4) return 400 + nameNorm.Length;
        var tokens = wantedNorm.Split(' ', StringSplitOptions.RemoveEmptyEntries).Where(token => token.Length >= 3).ToArray();
        if (tokens.Length == 0) return 0;
        var hit = tokens.Count(token => nameNorm.Contains(token, StringComparison.Ordinal));
        if (hit == 0) return 0;
        var score = hit * 80;
        if (hit == tokens.Length) score += 200;
        return score;
    }

    internal static bool LooksLikeForbiddenLaunchName(string application)
    {
        var value = (application ?? "").Trim();
        if (value.Length == 0) return true;
        if (value.Contains('\\') || value.Contains('/') || value.Contains(';')
            || value.Contains('|') || value.Contains('&') || value.Contains('>'))
            return true;
        if (value.Length >= 2 && char.IsLetter(value[0]) && value[1] == ':')
            return true;
        var extension = Path.GetExtension(value);
        return extension.Equals(".exe", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".cmd", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".bat", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".ps1", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".msi", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".com", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".scr", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".vbs", StringComparison.OrdinalIgnoreCase)
            || extension.Equals(".js", StringComparison.OrdinalIgnoreCase);
    }

    private static (string Name, string Path)? ResolveInstalledLaunch(string application, out string[] candidates)
    {
        candidates = [];
        if (LooksLikeForbiddenLaunchName(application))
            return null;
        if (LaunchTargets.TryGetValue(application, out var known))
            return (LaunchTargets.Keys.First(key => key.Equals(application, StringComparison.OrdinalIgnoreCase)), known);

        var installed = AllInstalledApplications();
        var exact = installed.FirstOrDefault(item => item.Name.Equals(application, StringComparison.OrdinalIgnoreCase));
        if (!string.IsNullOrWhiteSpace(exact.Path))
            return (exact.Name, exact.Path);

        var scored = installed
            .Select(item => (item.Name, item.Path, Score: ScoreInstalledName(application, item.Name)))
            .Where(item => item.Score >= 200)
            .OrderByDescending(item => item.Score)
            .ThenBy(item => item.Name.Length)
            .Take(5)
            .ToArray();
        candidates = scored.Select(item => item.Name).ToArray();
        if (scored.Length == 0) return null;
        var best = scored[0];
        var unique = scored.Length == 1
            || (best.Score >= 500 && (scored.Length < 2 || best.Score >= scored[1].Score + 150));
        return unique ? (best.Name, best.Path) : null;
    }

    private static object ResolveApplication(string name)
    {
        if (LooksLikeForbiddenLaunchName(name))
            throw new InvalidOperationException("An application name is required to resolve a window.");
        var match = FindApplicationProcess(name);
        if (match is not null)
        {
            try
            {
                return new
                {
                    processId = match.Id,
                    name = match.ProcessName ?? "",
                    title = Truncate(WindowTitle(FindBestWindowHandle(match, name)), 160),
                    matched = name
                };
            }
            finally { match.Dispose(); }
        }
        throw new InvalidOperationException($"No running application window matches \"{name}\".");
    }

    private static object LaunchApplication(HostRequest request)
    {
        var requested = GetApplication(request);
        var resolved = ResolveInstalledLaunch(requested, out var candidates);
        if (resolved is null)
        {
            throw new UnauthorizedAccessException(candidates.Length > 0
                ? $"{requested} is ambiguous. Choose one exact installed name: {string.Join(", ", candidates)}."
                : $"{requested} is not an installed application Alfred can launch.");
        }
        var application = resolved.Value.Name;
        var target = resolved.Value.Path;
        var existing = FindApplicationProcess(application);
        if (existing is not null)
        {
            try
            {
                try { FocusProcess(existing, application); }
                catch { /* Already running; the next observe/click retries focus. */ }
                return new
                {
                    launched = false,
                    alreadyRunning = true,
                    application,
                    requested,
                    processId = existing.Id,
                    title = Truncate(WindowTitle(FindBestWindowHandle(existing, application)), 160)
                };
            }
            finally { existing.Dispose(); }
        }

        StartInstalledTarget(target);
        var found = WaitForApplicationProcess(application, TimeSpan.FromSeconds(15))
            ?? throw new InvalidOperationException($"{application} launched without an accessible window.");
        try
        {
            // Store/Settings often start a stub, then hand the window to another PID.
            Thread.Sleep(400);
            var handedOff = FindApplicationProcess(application);
            if (handedOff is not null && handedOff.Id != found.Id)
            {
                found.Dispose();
                found = handedOff;
            }
            else
            {
                handedOff?.Dispose();
            }
            FocusProcess(found, application);
            return new
            {
                launched = true,
                application,
                requested,
                processId = found.Id,
                title = Truncate(WindowTitle(FindBestWindowHandle(found, application)), 160)
            };
        }
        finally { found.Dispose(); }
    }

    private static void StartInstalledTarget(string target)
    {
        if (target.StartsWith("aumid:", StringComparison.OrdinalIgnoreCase))
        {
            ActivateAppUserModelId(target["aumid:".Length..]);
            return;
        }
        if (target.Contains(':', StringComparison.Ordinal) && !Path.IsPathRooted(target)
            && !target.EndsWith(".lnk", StringComparison.OrdinalIgnoreCase)
            && !target.EndsWith(".exe", StringComparison.OrdinalIgnoreCase))
        {
            Process.Start(new ProcessStartInfo(target) { UseShellExecute = true })
                ?.Dispose();
            return;
        }
        Process.Start(new ProcessStartInfo(target) { UseShellExecute = true })
            ?.Dispose();
    }

    private static void ActivateAppUserModelId(string aumid)
    {
        if (string.IsNullOrWhiteSpace(aumid))
            throw new InvalidOperationException("The application is missing an AppUserModelID.");
        try
        {
            var activator = (IApplicationActivationManager)new ApplicationActivationManager();
            activator.ActivateApplication(aumid, string.Empty, ActivateOptions.None, out _);
            return;
        }
        catch { /* Fall back to the shell:AppsFolder protocol below. */ }
        var explorer = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Windows), "explorer.exe");
        Process.Start(new ProcessStartInfo
        {
            FileName = explorer,
            Arguments = "shell:AppsFolder\\" + aumid,
            UseShellExecute = true
        })?.Dispose();
    }

    private static Process? WaitForApplicationProcess(string application, TimeSpan timeout)
    {
        var deadline = DateTime.UtcNow + timeout;
        while (DateTime.UtcNow < deadline)
        {
            var found = FindApplicationProcess(application);
            if (found is not null && FindBestWindowHandle(found, application) != IntPtr.Zero)
                return found;
            found?.Dispose();
            Thread.Sleep(200);
        }
        return null;
    }

    private static IEnumerable<string> ProcessNamesFor(string application)
    {
        if (LaunchTargets.TryGetValue(application, out var target)
            && !target.Contains(':', StringComparison.Ordinal)
            && !target.StartsWith("aumid:", StringComparison.OrdinalIgnoreCase))
        {
            yield return Path.GetFileNameWithoutExtension(target);
        }
        if (ProcessAliases.TryGetValue(application, out var aliases))
        {
            foreach (var alias in aliases) yield return alias;
        }
    }

    private static Process? FindApplicationProcess(string application)
    {
        if (string.IsNullOrWhiteSpace(application) || LooksLikeForbiddenLaunchName(application))
            return null;
        var aliases = ProcessNamesFor(application)
            .Where(name => !string.IsNullOrWhiteSpace(name))
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        var wantBrowser = BrowserApplications.Contains(application);
        Process? match = null;
        var bestScore = 0;
        foreach (var window in EnumerateTopWindows())
        {
            try
            {
                using var process = Process.GetProcessById(window.ProcessId);
                var processName = process.ProcessName ?? "";
                if (processName.StartsWith("alfred", StringComparison.OrdinalIgnoreCase)) continue;
                var isBrowser = BrowserProcessNames.Contains(processName);
                if (isBrowser && !wantBrowser) continue;

                var score = 0;
                if (aliases.Contains(processName)) score += 80;
                var title = window.Title;
                if (!string.IsNullOrWhiteSpace(title))
                {
                    if (title.Equals(application, StringComparison.OrdinalIgnoreCase)) score += 100;
                    else if (title.StartsWith(application + " ", StringComparison.OrdinalIgnoreCase)
                        || title.StartsWith(application + " -", StringComparison.OrdinalIgnoreCase)
                        || title.EndsWith(" - " + application, StringComparison.OrdinalIgnoreCase))
                        score += 70;
                    else if (title.Contains(application, StringComparison.OrdinalIgnoreCase)) score += 40;
                    else
                    {
                        var lastWord = application.Split(' ', StringSplitOptions.RemoveEmptyEntries).LastOrDefault() ?? "";
                        if (lastWord.Length >= 4 && title.Equals(lastWord, StringComparison.OrdinalIgnoreCase))
                            score += 70;
                    }
                }
                if (processName.Equals("ApplicationFrameHost", StringComparison.OrdinalIgnoreCase))
                {
                    if (score < 70) continue;
                }
                else if (score < 80)
                    continue;
                if (score > bestScore || (score == bestScore && match is not null && process.Id > match.Id))
                {
                    match?.Dispose();
                    match = Process.GetProcessById(window.ProcessId);
                    bestScore = score;
                }
            }
            catch { /* The process exited or denies access; skip it. */ }
        }
        if (match is not null)
            RememberApplication(match.Id, application);
        return match;
    }

    private static readonly Dictionary<int, string> ApplicationNames = [];
    private static readonly object ApplicationNameGate = new();

    private static void RememberApplication(int processId, string? application)
    {
        if (string.IsNullOrWhiteSpace(application)
            || application.Equals("Alfred", StringComparison.OrdinalIgnoreCase))
            return;
        lock (ApplicationNameGate)
            ApplicationNames[processId] = application.Trim();
    }

    private static string? RememberedApplication(int processId)
    {
        lock (ApplicationNameGate)
            return ApplicationNames.TryGetValue(processId, out var name) ? name : null;
    }

    private static bool TitleMatchesApplication(string title, string application)
    {
        if (string.IsNullOrWhiteSpace(title) || string.IsNullOrWhiteSpace(application))
            return false;
        if (title.Equals(application, StringComparison.OrdinalIgnoreCase)
            || title.StartsWith(application + " ", StringComparison.OrdinalIgnoreCase)
            || title.StartsWith(application + " -", StringComparison.OrdinalIgnoreCase)
            || title.EndsWith(" - " + application, StringComparison.OrdinalIgnoreCase)
            || title.Contains(application, StringComparison.OrdinalIgnoreCase))
            return true;
        var titleNorm = NormalizeText(title);
        var appNorm = NormalizeText(application);
        if (appNorm.Length > 0 && (titleNorm == appNorm || titleNorm.Contains(appNorm)))
            return true;
        var lastWord = application.Split(' ', StringSplitOptions.RemoveEmptyEntries).LastOrDefault() ?? "";
        return lastWord.Length >= 4
            && (title.Equals(lastWord, StringComparison.OrdinalIgnoreCase)
                || titleNorm == NormalizeText(lastWord));
    }

    private static bool IsApplicationFrameHost(int processId)
    {
        try
        {
            using var process = Process.GetProcessById(processId);
            return (process.ProcessName ?? "").Equals("ApplicationFrameHost", StringComparison.OrdinalIgnoreCase);
        }
        catch { return false; }
    }

    private static IntPtr FindBestWindowHandle(Process process, string? application)
    {
        process.Refresh();
        if (string.IsNullOrWhiteSpace(application))
            application = RememberedApplication(process.Id);
        var windows = EnumerateTopWindows()
            .Where(window => window.ProcessId == process.Id)
            .ToArray();
        if (!string.IsNullOrWhiteSpace(application))
        {
            var titled = windows.FirstOrDefault(window => TitleMatchesApplication(window.Title, application));
            if (titled.Handle != IntPtr.Zero) return titled.Handle;
            var frame = EnumerateTopWindows().FirstOrDefault(window =>
                IsApplicationFrameHost(window.ProcessId)
                && TitleMatchesApplication(window.Title, application));
            if (frame.Handle != IntPtr.Zero)
            {
                RememberApplication(frame.ProcessId, application);
                return frame.Handle;
            }
        }
        if (windows.Length == 0)
            return process.MainWindowHandle;
        if (process.MainWindowHandle != IntPtr.Zero
            && windows.Any(window => window.Handle == process.MainWindowHandle))
            return process.MainWindowHandle;
        return windows[0].Handle;
    }

    private static bool ElementBelongsToProcess(AutomationElement? element, Process process, string? application = null)
    {
        if (element is null) return false;
        try
        {
            var elementPid = element.Current.ProcessId;
            if (elementPid == process.Id) return true;
            application = string.IsNullOrWhiteSpace(application)
                ? RememberedApplication(process.Id)
                : application;
            var handle = FindBestWindowHandle(process, application);
            if (handle == IntPtr.Zero) return false;
            GetWindowThreadProcessId(handle, out var handlePid);
            if (handlePid != 0 && elementPid == (int)handlePid) return true;
            var walker = TreeWalker.ControlViewWalker;
            var current = element;
            for (var depth = 0; depth < 24 && current is not null; depth++)
            {
                var native = current.Current.NativeWindowHandle;
                if (native != 0 && new IntPtr(native) == handle)
                    return true;
                current = walker.GetParent(current);
            }
        }
        catch { /* Stale UIA nodes are treated as not belonging. */ }
        return false;
    }

    private static IntPtr RequireWindowHandle(Process process, string? application)
    {
        var handle = FindBestWindowHandle(process, application);
        if (handle == IntPtr.Zero)
            throw new InvalidOperationException("The application does not have an accessible window.");
        return handle;
    }

    private static AutomationElement RequireAutomationRoot(Process process, string? application) =>
        TryAutomationRoot(process, application)
            ?? throw new InvalidOperationException("The application window is unavailable.");

    private static AutomationElement? TryAutomationRoot(Process process, string? application)
    {
        var handle = FindBestWindowHandle(process, application);
        return handle == IntPtr.Zero ? null : AutomationElement.FromHandle(handle);
    }

    private static string WindowTitle(IntPtr handle)
    {
        if (handle == IntPtr.Zero) return "";
        var buffer = new StringBuilder(512);
        return GetWindowText(handle, buffer, buffer.Capacity) > 0 ? buffer.ToString() : "";
    }

    private readonly record struct TopWindow(int ProcessId, IntPtr Handle, string Title);

    private static List<TopWindow> EnumerateTopWindows()
    {
        var windows = new List<TopWindow>();
        EnumWindows((handle, _) =>
        {
            if (!IsWindowVisible(handle) || GetWindowTextLength(handle) == 0) return true;
            GetWindowThreadProcessId(handle, out var processId);
            if (processId == 0) return true;
            var title = WindowTitle(handle);
            if (string.IsNullOrWhiteSpace(title)) return true;
            windows.Add(new TopWindow((int)processId, handle, title));
            return true;
        }, IntPtr.Zero);
        return windows;
    }

    private static object WaitFor(HostRequest request)
    {
        var text = (GetOptionalString(request.Params, "text") ?? request.Target ?? "").Trim();
        if (text.Length == 0)
            throw new InvalidOperationException("wait requires text to look for.");
        var timeoutMs = Math.Clamp(GetOptionalInt(request.Params, "timeoutMs") ?? 8000, 250, 20_000);
        var started = DateTime.UtcNow;
        var deadline = started.AddMilliseconds(timeoutMs);
        var application = GetApplication(request);
        while (DateTime.UtcNow < deadline)
        {
            try
            {
                var process = ResolveProcess(request);
                var matches = FindMarksByText(process, application, text);
                if (matches.Count > 0)
                {
                    return new
                    {
                        found = true,
                        waitedMs = (int)(DateTime.UtcNow - started).TotalMilliseconds,
                        text,
                        mark = matches[0].Id,
                        count = matches.Count,
                        matches = matches.Select(MarkJson).ToArray()
                    };
                }
                var title = WindowTitle(FindBestWindowHandle(process, application));
                if (title.Contains(text, StringComparison.OrdinalIgnoreCase))
                {
                    return new
                    {
                        found = true,
                        waitedMs = (int)(DateTime.UtcNow - started).TotalMilliseconds,
                        text,
                        mark = (string?)null,
                        count = 0,
                        title
                    };
                }
            }
            catch { /* Window may still be creating; keep polling. */ }
            Thread.Sleep(250);
        }
        return new
        {
            found = false,
            waitedMs = timeoutMs,
            text,
            mark = (string?)null,
            count = 0
        };
    }

    [Flags]
    private enum ActivateOptions
    {
        None = 0
    }

    [ComImport]
    [Guid("2e941141-7f97-4756-ba1d-9decde894a3d")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IApplicationActivationManager
    {
        IntPtr ActivateApplication(
            [In, MarshalAs(UnmanagedType.LPWStr)] string appUserModelId,
            [In, MarshalAs(UnmanagedType.LPWStr)] string arguments,
            [In] ActivateOptions options,
            [Out] out uint processId);
    }

    [ComImport]
    [Guid("45BA127D-10A8-46EA-8AB7-56EA9078943C")]
    private class ApplicationActivationManager
    {
    }

    private delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr hWnd);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowTextLength(IntPtr hWnd);
}
