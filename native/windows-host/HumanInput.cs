using System.Diagnostics;
using System.Runtime.InteropServices;

internal static partial class Program
{
    // Mark-targeted virtual mouse and keyboard. The planner never aims raw
    // pixels; every gesture is aimed at a resolved control or a window-relative
    // probe point, and aborts if the target window moves or loses focus.

    [StructLayout(LayoutKind.Sequential)]
    private struct POINT
    {
        public int X;
        public int Y;
    }

    [DllImport("user32.dll")]
    private static extern bool GetCursorPos(out POINT point);

    private static (int X, int Y) CurrentCursor()
    {
        GetCursorPos(out var point);
        return (point.X, point.Y);
    }

    private static bool PointInProcessWindow(int processId, int x, int y)
    {
        try
        {
            using var process = Process.GetProcessById(processId);
            if (!GetWindowRect(process.MainWindowHandle, out var rect)) return false;
            return x >= rect.Left && x < rect.Right && y >= rect.Top && y < rect.Bottom;
        }
        catch
        {
            return false;
        }
    }

    private static void RequireForeground(int processId)
    {
        var foreground = GetForegroundWindow();
        GetWindowThreadProcessId(foreground, out var foregroundPid);
        if (foregroundPid != (uint)processId)
            throw new InvalidOperationException("The target window lost foreground focus; input was not sent.");
    }

    private static void MoveCursorHuman(int processId, int toX, int toY)
    {
        RequireInsideWindow(processId, toX, toY);
        var (fromX, fromY) = CurrentCursor();
        var dx = toX - fromX;
        var dy = toY - fromY;
        var distance = Math.Sqrt(dx * (double)dx + dy * (double)dy);
        if (distance < 4)
        {
            SetCursorPos(toX, toY);
            return;
        }

        var steps = Math.Clamp((int)Math.Round(distance / 16.0), 8, 32);
        var arc = Math.Min(72.0, distance * 0.14);
        var midX = fromX + dx / 2.0 - Math.CopySign(arc * 0.35, dy == 0 ? 1 : dy);
        var midY = fromY + dy / 2.0 + Math.CopySign(arc * 0.35, dx == 0 ? 1 : dx);
        var entered = PointInProcessWindow(processId, fromX, fromY);

        for (var step = 1; step <= steps; step++)
        {
            var t = step / (double)steps;
            var eased = t < 0.5 ? 2 * t * t : 1 - Math.Pow(-2 * t + 2, 2) / 2.0;
            var remain = 1 - eased;
            var x = (int)Math.Round(remain * remain * fromX + 2 * remain * eased * midX + eased * eased * toX);
            var y = (int)Math.Round(remain * remain * fromY + 2 * remain * eased * midY + eased * eased * toY);
            if (entered && !PointInProcessWindow(processId, x, y))
            {
                x = toX;
                y = toY;
            }
            SetCursorPos(x, y);
            if (PointInProcessWindow(processId, x, y)) entered = true;
            Thread.Sleep(7);
            if (step == steps / 2 || step == steps)
                RequireForeground(processId);
        }

        RequireInsideWindow(processId, toX, toY);
        SetCursorPos(toX, toY);
        Thread.Sleep(18);
    }

    private static void HumanLeftClick(int processId, int x, int y)
    {
        MoveCursorHuman(processId, x, y);
        RequireForeground(processId);
        RequireInsideWindow(processId, x, y);
        Send([MouseInput(MOUSEEVENTF_LEFTDOWN)]);
        Thread.Sleep(55);
        RequireForeground(processId);
        Send([MouseInput(MOUSEEVENTF_LEFTUP)]);
    }

    private static void HumanRightClick(int processId, int x, int y)
    {
        MoveCursorHuman(processId, x, y);
        RequireForeground(processId);
        RequireInsideWindow(processId, x, y);
        Send([MouseInput(MOUSEEVENTF_RIGHTDOWN)]);
        Thread.Sleep(55);
        RequireForeground(processId);
        Send([MouseInput(MOUSEEVENTF_RIGHTUP)]);
    }

    private static void HumanDoubleClick(int processId, int x, int y)
    {
        HumanLeftClick(processId, x, y);
        Thread.Sleep(40);
        RequireInsideWindow(processId, x, y);
        RequireForeground(processId);
        Send([MouseInput(MOUSEEVENTF_LEFTDOWN)]);
        Thread.Sleep(40);
        Send([MouseInput(MOUSEEVENTF_LEFTUP)]);
    }

    private static void HumanHover(int processId, int x, int y)
    {
        MoveCursorHuman(processId, x, y);
        RequireForeground(processId);
        RequireInsideWindow(processId, x, y);
        Thread.Sleep(260);
    }

    private static void HumanDrag(int processId, int fromX, int fromY, int toX, int toY)
    {
        MoveCursorHuman(processId, fromX, fromY);
        RequireForeground(processId);
        RequireInsideWindow(processId, fromX, fromY);
        Send([MouseInput(MOUSEEVENTF_LEFTDOWN)]);
        Thread.Sleep(40);
        MoveCursorHuman(processId, toX, toY);
        RequireForeground(processId);
        RequireInsideWindow(processId, toX, toY);
        Thread.Sleep(30);
        Send([MouseInput(MOUSEEVENTF_LEFTUP)]);
    }

    private static void HumanWheel(int processId, int x, int y, int delta, bool horizontal)
    {
        MoveCursorHuman(processId, x, y);
        RequireForeground(processId);
        RequireInsideWindow(processId, x, y);
        Send([WheelInput(delta, horizontal)]);
    }

    private static void HumanTypeCharacters(string text)
    {
        foreach (var character in text)
        {
            Send([KeyboardInput(character, false), KeyboardInput(character, true)]);
            // Deterministic cadence (15–40 ms) so retries stay reproducible.
            Thread.Sleep(18 + (character % 15));
        }
    }

    private static void HumanPressVirtualKey(ushort virtualKey)
    {
        Send([VirtualKeyInput(virtualKey, false)]);
        Thread.Sleep(35);
        Send([VirtualKeyInput(virtualKey, true)]);
    }

    private static void HumanPressShortcut(ushort virtualKey)
    {
        const ushort control = 0x11;
        Send([VirtualKeyInput(control, false)]);
        Thread.Sleep(25);
        Send([VirtualKeyInput(virtualKey, false)]);
        Thread.Sleep(35);
        Send([VirtualKeyInput(virtualKey, true)]);
        Thread.Sleep(20);
        Send([VirtualKeyInput(control, true)]);
    }
}
