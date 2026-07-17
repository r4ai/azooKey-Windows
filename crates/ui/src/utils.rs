use tao::window::Window;
use windows::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST},
};

pub fn calculate_candidate_window_position(
    anchor: (i32, i32, i32, i32),
    window_size: (u32, u32),
    work_area: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (top, left, bottom, _) = anchor;
    let (width, height) = (i64::from(window_size.0), i64::from(window_size.1));
    let (work_left, work_top, work_right, work_bottom) = (
        i64::from(work_area.0),
        i64::from(work_area.1),
        i64::from(work_area.2),
        i64::from(work_area.3),
    );

    let clamp_to_axis = |value: i64, minimum: i64, maximum: i64| {
        if maximum < minimum {
            minimum
        } else {
            value.clamp(minimum, maximum)
        }
    };

    let x = clamp_to_axis(i64::from(left) - 15, work_left, work_right - width);
    let below = i64::from(bottom);
    let y = if below + height > work_bottom {
        i64::from(top) - height
    } else {
        below
    };
    let y = clamp_to_axis(y, work_top, work_bottom - height);

    (
        i32::try_from(x).unwrap_or(if x < 0 { i32::MIN } else { i32::MAX }),
        i32::try_from(y).unwrap_or(if y < 0 { i32::MIN } else { i32::MAX }),
    )
}

pub fn get_candidate_window_position(
    top: i32,
    left: i32,
    bottom: i32,
    right: i32,
    window: &Window,
) -> (f64, f64) {
    let monitor = unsafe {
        MonitorFromRect(
            &RECT {
                left,
                top,
                right,
                bottom,
            } as *const _,
            MONITOR_DEFAULTTONEAREST,
        )
    };

    let mut monitor_info = MONITORINFO::default();
    monitor_info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;

    unsafe {
        let _ = GetMonitorInfoW(monitor, &mut monitor_info);
    }

    let size = window.inner_size();
    let (x, y) = calculate_candidate_window_position(
        (top, left, bottom, right),
        (size.width, size.height),
        (
            monitor_info.rcWork.left,
            monitor_info.rcWork.top,
            monitor_info.rcWork.right,
            monitor_info.rcWork.bottom,
        ),
    );

    (x as f64, y as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_uses_the_space_below_the_caret_when_it_fits() {
        assert_eq!(
            calculate_candidate_window_position(
                (100, 200, 120, 220),
                (225, 180),
                (0, 0, 1920, 1040),
            ),
            (185, 120)
        );
    }

    #[test]
    fn placement_moves_above_the_caret_near_the_bottom_edge() {
        assert_eq!(
            calculate_candidate_window_position(
                (900, 200, 920, 220),
                (225, 180),
                (0, 0, 1920, 1040),
            ),
            (185, 720)
        );
    }

    #[test]
    fn placement_stays_inside_the_work_area_when_neither_side_is_tall_enough() {
        assert_eq!(
            calculate_candidate_window_position(
                (140, 200, 160, 220),
                (225, 180),
                (0, 0, 1920, 300),
            ),
            (185, 0)
        );
    }

    #[test]
    fn placement_stays_inside_the_horizontal_work_area() {
        assert_eq!(
            calculate_candidate_window_position(
                (100, 1900, 120, 1920),
                (225, 180),
                (0, 0, 1920, 1040),
            ),
            (1695, 120)
        );
        assert_eq!(
            calculate_candidate_window_position(
                (100, -50, 120, -30),
                (225, 180),
                (0, 0, 1920, 1040),
            ),
            (0, 120)
        );
        assert_eq!(
            calculate_candidate_window_position((100, 300, 120, 320), (720, 180), (0, 0, 640, 480),),
            (0, 120)
        );
    }
}
