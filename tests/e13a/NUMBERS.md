# The numbers (E13a, then W1)

Made by `python3 tests/e13a/suite.py table` from `results/`. "Released" is the app as released (0.40.12's `src/`): every mode, two passes, and a third stalled run. "W1" is this branch: every mode and its hooks, two passes, and a third stalled run. Pass 2 ran the two builds in turn, mode by mode. Times are in seconds. The slack, and the reason for it, are in `suite.py` (`SLACK`) and in the report.

| Test | Measure | Released: each run | Released: slowest | Allowed (slack) | W1: each run | W1: slowest | Within |
|---|---|---|---|---|---|---|---|
| `cold_start_publishes_the_saved_account` | launch_to_home_drawn | 0.718, 0.726, 0.745, 0.788, 0.759, 0.720, 0.738, 0.751, 0.756, 0.705, 0.734, 0.709, 0.870, 0.743, 0.866, 0.715, 0.729 | 0.870 | 1.120 (ui) | 0.727, 0.726, 0.763, 0.764, 0.703, 0.732, 0.725, 0.711, 0.712, 0.715, 0.705, 0.728, 0.700, 0.717, 0.721, 0.692, 0.716, 0.717, 0.719, 0.706, 0.711, 0.728, 0.735, 0.777, 0.748, 0.716, 0.731, 0.716, 0.719 | 0.777 | yes |
| `cold_start_puts_the_queue_back_and_plays` | launch_to_queue_shown | 0.740, 0.751, 0.773, 0.822, 0.784, 0.741, 0.759, 0.777, 0.785, 0.731, 0.758, 0.734, 0.892, 0.769, 0.890, 0.736, 0.751 | 0.892 | 1.142 (ui) | 0.756, 0.751, 0.788, 0.789, 0.723, 0.754, 0.751, 0.737, 0.737, 0.739, 0.729, 0.754, 0.725, 0.740, 0.747, 0.712, 0.742, 0.744, 0.744, 0.731, 0.736, 0.751, 0.754, 0.803, 0.772, 0.741, 0.756, 0.744, 0.744 | 0.803 | yes |
| `cold_start_puts_the_queue_back_and_plays` | play_to_sound | 0.085, 0.088, 0.088, 0.089, 0.086, 0.084, 0.085, 0.117, 0.088, 0.086, 0.087, 0.083, 0.086, 0.091, 0.094, 0.085, 0.084 | 0.117 | 0.367 (file) | 0.088, 0.086, 0.087, 0.087, 0.086, 0.083, 0.086, 0.085, 0.087, 0.086, 0.085, 0.092, 0.087, 0.088, 0.086, 0.083, 0.086, 0.086, 0.087, 0.087, 0.087, 0.084, 0.086, 0.086, 0.087, 0.086, 0.086, 0.086, 0.086 | 0.092 | yes |
| `a_play_is_counted_as_released` | play_to_counted | 10.500, 10.323, 10.497, 10.527, 10.480, 10.506, 10.494, 10.383, 10.474, 10.495, 10.489, 10.485, 10.486, 10.474, 10.488, 10.482, 10.473 | 10.527 | 11.527 (timer) | 10.476, 10.487, 10.429, 10.439, 10.483, 10.487, 10.488, 10.490, 10.483, 10.521, 10.496, 10.312, 10.492, 10.317, 10.486, 10.482, 10.480, 10.497, 10.511, 10.486, 10.484, 10.465, 10.501, 10.421, 10.435, 10.488, 10.471, 10.478, 10.477 | 10.521 | yes |
| `a_like_and_a_play_reach_the_music_server` | counted_to_server_has_play | 0.309, 1.227, 0.483, 0.616, 0.309, 0.615, 0.308, 1.128, 0.422, 0.312, 0.308, 0.310, 0.312, 0.309, 0.308, 0.309, 0.307 | 1.227 | 2.227 (timer) | 0.309, 0.312, 0.310, 0.308, 0.308, 0.309, 0.308, 0.308, 0.308, 0.410, 0.308, 0.410, 0.309, 0.411, 0.308, 0.307, 0.311, 0.309, 0.309, 0.311, 0.311, 0.311, 0.512, 0.309, 0.308, 0.308, 0.309, 0.308, 0.318 | 0.512 | yes |
| `a_like_and_a_play_reach_the_music_server` | like_to_server | 0.114, 0.058, 0.061, 0.058, 0.058, 0.066, 0.118, 0.117, 0.064, 0.062, 0.114, 0.057, 0.058, 0.116, 0.056, 0.058, 0.057 | 0.118 | 0.368 (file) | 0.065, 0.058, 0.058, 0.057, 0.058, 0.057, 0.058, 0.058, 0.059, 0.057, 0.057, 0.057, 0.112, 0.113, 0.064, 0.058, 0.114, 0.056, 0.057, 0.113, 0.058, 0.058, 0.056, 0.057, 0.056, 0.057, 0.057, 0.111, 0.057 | 0.114 | yes |
| `a_like_and_a_play_reach_the_music_server` | server_back_to_play_told_again | 2.996, 2.892, 3.087, 2.790, 2.904, 2.901, 2.890, 2.998, 2.809, 2.886, 2.881, 3.007, 2.884, 2.931, 2.626, 3.092, 2.676 | 3.092 | 4.092 (timer) | 3.203, 3.001, 2.795, 2.886, 2.895, 2.984, 3.001, 2.895, 3.084, 3.015, 2.909, 3.091, 2.893, 2.998, 3.079, 2.889, 2.998, 3.103, 3.089, 2.795, 2.968, 2.997, 3.009, 2.995, 3.184, 2.884, 2.994, 2.800, 2.880 | 3.203 | yes |
| `downloads_start_and_resume` | launch_to_list_scheduled_its_own | 11.551, 12.303, 11.755, 11.967, 11.575, 11.864, 11.563, 12.290, 11.684, 11.540, 11.557, 11.531, 11.692, 11.556, 11.687, 11.529, 11.534 | 12.303 | 13.303 (timer) | 11.543, 11.553, 11.530, 11.538, 11.517, 11.554, 11.550, 11.537, 11.530, 11.673, 11.536, 11.478, 11.529, 11.471, 11.542, 11.504, 11.535, 11.553, 11.567, 11.529, 11.533, 11.529, 11.770, 11.536, 11.517, 11.539, 11.537, 11.532, 11.540 | 11.770 | yes |
| `downloads_start_and_resume` | by_hand_8_songs | 0.214, 0.242, 0.249, 0.268, 0.230, 0.227, 0.218, 0.249, 0.286, 0.215, 0.219, 0.217, 0.266, 0.224, 0.471, 0.217, 0.200 | 0.471 | 0.721 (file) | 0.231, 0.212, 0.217, 0.224, 0.224, 0.207, 0.220, 0.224, 0.219, 0.269, 0.229, 0.221, 0.208, 0.266, 0.214, 0.211, 0.215, 0.275, 0.212, 0.214, 0.409, 0.225, 0.204, 0.211, 0.223, 0.223, 0.229, 0.218, 0.215 | 0.409 | yes |
| `downloads_start_and_resume` | server_back_to_downloads_done | 22.818, 22.711, 22.822, 22.753, 22.786, 22.710, 22.813, 22.793, 22.791, 22.768, 22.701, 22.733, 22.912, 22.805, 22.856, 22.811, 22.776 | 22.912 | 23.912 (timer) | 22.789, 22.679, 22.790, 22.778, 22.792, 22.891, 22.690, 22.778, 21.454, 22.825, 22.712, 22.870, 22.785, 22.707, 22.798, 22.719, 22.796, 22.736, 21.452, 22.790, 22.853, 22.780, 22.738, 22.721, 22.694, 22.796, 22.693, 22.846, 22.776 | 22.891 | yes |
| `the_queue_is_changed_and_kept` | add_to_kept | 0.155, 0.157, 0.181, 0.163, 0.180, 0.154, 0.190, 0.168, 0.183, 0.155, 0.156, 0.151, 0.163, 0.163, 0.184, 0.163, 0.133 | 0.190 | 0.440 (file) | 0.149, 0.218, 0.159, 0.155, 0.155, 0.151, 0.148, 0.156, 0.154, 0.148, 0.153, 0.150, 0.156, 0.145, 0.156, 0.156, 0.155, 0.148, 0.153, 0.153, 0.155, 0.156, 0.153, 0.149, 0.152, 0.155, 0.148, 0.146, 0.157 | 0.218 | yes |
| `the_queue_is_changed_and_kept` | move_to_kept | 0.134, 0.136, 0.156, 0.131, 0.135, 0.135, 0.138, 0.151, 0.140, 0.129, 0.135, 0.132, 0.135, 0.134, 0.142, 0.134, 0.121 | 0.156 | 0.406 (file) | 0.133, 0.143, 0.135, 0.128, 0.129, 0.129, 0.130, 0.136, 0.133, 0.129, 0.131, 0.130, 0.131, 0.131, 0.134, 0.134, 0.135, 0.134, 0.133, 0.132, 0.133, 0.133, 0.132, 0.129, 0.131, 0.132, 0.128, 0.129, 0.134 | 0.143 | yes |
| `the_queue_is_changed_and_kept` | remove_to_kept | 0.136, 0.135, 0.140, 0.142, 0.135, 0.134, 0.136, 0.138, 0.171, 0.131, 0.133, 0.133, 0.130, 0.138, 0.142, 0.135, 0.122 | 0.171 | 0.421 (file) | 0.135, 0.142, 0.134, 0.128, 0.134, 0.130, 0.130, 0.134, 0.134, 0.130, 0.130, 0.130, 0.134, 0.128, 0.134, 0.134, 0.136, 0.134, 0.130, 0.131, 0.130, 0.134, 0.129, 0.129, 0.133, 0.133, 0.128, 0.128, 0.134 | 0.142 | yes |
| `the_queue_is_changed_and_kept` | restart_to_queue_shown | 0.745, 0.724, 0.857, 0.770, 0.734, 0.740, 0.722, 0.796, 0.715, 0.735, 0.784, 0.717, 0.774, 0.796, 0.812, 0.728, 0.706 | 0.857 | 1.107 (ui) | 0.721, 0.778, 0.755, 0.758, 0.743, 0.743, 0.744, 0.764, 0.723, 0.744, 0.704, 0.729, 0.702, 0.727, 0.728, 0.735, 0.737, 0.739, 0.731, 0.727, 0.744, 0.753, 0.701, 0.777, 0.771, 0.714, 0.732, 0.719, 0.726 | 0.778 | yes |
| `the_queue_is_changed_and_kept` | clear_to_kept | 0.074, 0.073, 0.128, 0.075, 0.073, 0.072, 0.070, 0.068, 0.089, 0.070, 0.070, 0.071, 0.075, 0.081, 0.077, 0.069, 0.064 | 0.128 | 0.378 (file) | 0.069, 0.091, 0.069, 0.072, 0.073, 0.069, 0.075, 0.075, 0.070, 0.076, 0.068, 0.073, 0.066, 0.069, 0.072, 0.069, 0.071, 0.075, 0.071, 0.071, 0.070, 0.071, 0.065, 0.069, 0.069, 0.069, 0.071, 0.072, 0.074 | 0.091 | yes |
| `a_direct_play_starts_it_cold` | space_to_sound | 0.023, 0.023, 0.026, 0.022, 0.023, 0.031, 0.023, 0.023, 0.024, 0.023, 0.022, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023 | 0.031 | 0.281 (file) | 0.023, 0.025, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.023, 0.024, 0.023, 0.023, 0.023, 0.023, 0.022, 0.024, 0.023, 0.023, 0.023, 0.023, 0.023 | 0.025 | yes |
| `two_songs_join_without_a_gap` | streamed_play_to_sound | 0.420, 0.419, 0.455, 0.424, 0.399, 0.400, 0.402, 0.184, 0.438, 0.422, 0.402, 0.404, 0.418, 0.421, 0.449, 0.404, 0.369 | 0.455 | 0.705 (file) | 0.402, 0.209, 0.404, 0.401, 0.408, 0.403, 0.405, 0.397, 0.401, 0.425, 0.382, 0.400, 0.386, 0.402, 0.399, 0.403, 0.400, 0.420, 0.401, 0.402, 0.402, 0.403, 0.366, 0.401, 0.384, 0.382, 0.414, 0.408, 0.397 | 0.425 | yes |
| `two_songs_join_without_a_gap` | streamed_longest_stall_around_the_join | 0.022, 0.022, 0.033, 0.025, 0.022, 0.022, 0.025, 0.036, 0.026, 0.025, 0.022, 0.022, 0.023, 0.024, 0.023, 0.024, 0.023 | 0.036 | 0.056 (gap) | 0.022, 0.023, 0.023, 0.022, 0.022, 0.024, 0.021, 0.022, 0.022, 0.023, 0.022, 0.022, 0.022, 0.023, 0.022, 0.025, 0.023, 0.024, 0.022, 0.022, 0.022, 0.024, 0.022, 0.022, 0.024, 0.023, 0.022, 0.021, 0.022 | 0.025 | yes |
| `two_songs_join_without_a_gap` | cached_play_to_sound | 0.190, 0.195, 0.186, 0.200, 0.192, 0.188, 0.189, 0.194, 0.293, 0.179, 0.197, 0.162, 0.194, 0.196, 0.189, 0.196, 0.162 | 0.293 | 0.543 (file) | 0.182, 0.175, 0.184, 0.168, 0.193, 0.174, 0.180, 0.185, 0.188, 0.192, 0.176, 0.159, 0.173, 0.180, 0.191, 0.194, 0.182, 0.202, 0.182, 0.186, 0.181, 0.191, 0.149, 0.187, 0.163, 0.184, 0.179, 0.187, 0.186 | 0.202 | yes |
| `two_songs_join_without_a_gap` | cached_longest_stall_around_the_join | 0.024, 0.022, 0.023, 0.025, 0.022, 0.022, 0.025, 0.064, 0.027, 0.023, 0.022, 0.024, 0.024, 0.023, 0.027, 0.023, 0.022 | 0.064 | 0.084 (gap) | 0.023, 0.022, 0.022, 0.022, 0.022, 0.022, 0.022, 0.023, 0.022, 0.023, 0.022, 0.023, 0.022, 0.024, 0.022, 0.024, 0.023, 0.023, 0.022, 0.022, 0.026, 0.024, 0.022, 0.022, 0.022, 0.022, 0.022, 0.022, 0.022 | 0.026 | yes |
| `two_songs_join_without_a_gap` | downloaded_play_to_sound | 0.190, 0.197, 0.191, 0.170, 0.188, 0.183, 0.180, 0.199, 0.206, 0.198, 0.186, 0.200, 0.200, 0.201, 0.210, 0.177, 0.166 | 0.210 | 0.460 (file) | 0.188, 0.189, 0.185, 0.180, 0.193, 0.207, 0.167, 0.216, 0.183, 0.199, 0.193, 0.176, 0.190, 0.186, 0.191, 0.208, 0.193, 0.209, 0.189, 0.182, 0.455, 0.180, 0.174, 0.184, 0.192, 0.185, 0.183, 0.197, 0.191 | 0.455 | yes |
| `two_songs_join_without_a_gap` | downloaded_longest_stall_around_the_join | 0.021, 0.023, 0.024, 0.022, 0.026, 0.023, 0.023, 0.022, 0.034, 0.034, 0.022, 0.022, 0.025, 0.034, 0.044, 0.024, 0.033 | 0.044 | 0.064 (gap) | 0.024, 0.021, 0.022, 0.027, 0.023, 0.035, 0.022, 0.023, 0.023, 0.024, 0.027, 0.022, 0.025, 0.024, 0.022, 0.038, 0.022, 0.023, 0.022, 0.022, 0.022, 0.024, 0.024, 0.022, 0.022, 0.042, 0.022, 0.022, 0.022 | 0.042 | yes |
| `offline_by_hand_and_by_loss` | by_hand_switch_to_offline | 0.072, 0.096, 0.083, 0.084, 0.099, 0.088, 0.080, 0.085, 0.083, 0.077, 0.078, 0.072, 0.086, 0.071, 0.228, 0.082, 0.072 | 0.228 | 0.478 (file) | 0.075, 0.078, 0.079, 0.083, 0.081, 0.070, 0.080, 0.078, 0.078, 0.082, 0.079, 0.081, 0.076, 0.097, 0.082, 0.073, 0.084, 0.079, 0.080, 0.081, 0.106, 0.077, 0.082, 0.085, 0.079, 0.068, 0.076, 0.095, 0.080 | 0.106 | yes |
| `offline_by_hand_and_by_loss` | offline_play_downloaded_to_sound | 0.665, 0.645, 0.645, 0.615, 0.659, 0.649, 0.656, 0.645, 0.667, 0.660, 0.655, 0.641, 0.656, 0.641, 0.680, 0.653, 0.625 | 0.680 | 0.930 (file) | 0.654, 0.634, 0.646, 0.644, 0.651, 0.644, 0.629, 0.652, 0.642, 0.655, 0.650, 0.645, 0.624, 0.641, 0.636, 0.652, 0.634, 0.654, 0.657, 0.658, 0.636, 0.660, 0.605, 0.633, 0.646, 0.640, 0.637, 0.649, 0.641 | 0.660 | yes |
| `offline_by_hand_and_by_loss` | by_hand_switch_back | 0.099, 0.094, 0.083, 0.089, 0.091, 0.093, 0.087, 0.111, 0.112, 0.088, 0.096, 0.087, 0.093, 0.086, 0.109, 0.102, 0.079 | 0.112 | 0.362 (file) | 0.081, 0.091, 0.082, 0.090, 0.103, 0.086, 0.091, 0.093, 0.091, 0.103, 0.090, 0.086, 0.092, 0.086, 0.093, 0.085, 0.100, 0.090, 0.089, 0.084, 0.102, 0.100, 0.106, 0.099, 0.096, 0.095, 0.091, 0.087, 0.090 | 0.106 | yes |
| `offline_by_hand_and_by_loss` | by_loss_noticed | 0.700, 0.699, 0.711, 0.755, 0.725, 0.740, 0.705, 0.698, 0.807, 0.738, 0.776, 0.710, 0.797, 0.701, 0.973, 0.743, 0.699 | 0.973 | 1.223 (ui) | 0.682, 0.690, 0.758, 0.755, 0.752, 0.749, 0.740, 0.688, 0.691, 0.706, 0.687, 0.748, 0.697, 0.668, 0.701, 0.746, 0.694, 0.699, 0.730, 0.740, 0.777, 0.698, 0.727, 0.732, 0.669, 0.732, 0.671, 0.743, 0.767 | 0.777 | yes |
| `offline_by_hand_and_by_loss` | by_loss_play_downloaded_to_sound | 1.765, 1.783, 1.801, 1.796, 1.846, 1.791, 1.804, 1.789, 1.875, 1.770, 1.814, 1.820, 1.823, 1.808, 1.872, 1.799, 1.715 | 1.875 | 2.125 (file) | 1.762, 1.747, 1.809, 1.793, 1.791, 1.792, 1.788, 1.800, 1.797, 1.811, 1.785, 1.773, 1.768, 1.778, 1.803, 1.802, 1.790, 1.802, 1.772, 1.817, 1.799, 1.806, 1.745, 1.755, 1.767, 1.783, 1.761, 1.783, 1.813 | 1.817 | yes |
| `offline_by_hand_and_by_loss` | server_back_to_online | 3.031, 2.927, 3.117, 2.818, 2.942, 2.937, 2.926, 3.026, 2.850, 2.920, 2.915, 3.042, 2.925, 2.977, 2.661, 3.130, 2.698 | 3.130 | 4.130 (timer) | 3.238, 3.040, 2.830, 2.920, 2.952, 3.016, 3.028, 2.927, 3.118, 3.056, 2.945, 3.125, 2.927, 3.034, 3.115, 2.923, 3.028, 3.135, 3.122, 2.828, 3.003, 3.037, 3.039, 3.031, 3.218, 2.918, 3.027, 2.838, 2.914 | 3.238 | yes |
| `an_account_change_is_not_held_up` | connect_to_home_drawn | 0.130, 0.128, 0.127, 0.122, 0.154, 0.138, 0.142, 0.137, 0.129, 0.127, 0.125, 0.133, 0.129, 0.127, 0.163, 0.135, 0.142 | 0.163 | 0.413 (ui) | 0.122, 0.128, 0.127, 0.129, 0.123, 0.112, 0.124, 0.120, 0.124, 0.146, 0.113, 0.130, 0.127, 0.130, 0.124, 0.113, 0.136, 0.150, 0.112, 0.135, 0.160, 0.155, 0.124, 0.124, 0.123, 0.126, 0.121, 0.118, 0.112 | 0.160 | yes |

| Run | Load at start, end | Failed | Log line (time as seconds after Play) | Queue kept through kill -9 | Settings row | Under /nexus/ at the proxy | At the stand-in |
|---|---|---|---|---|---|---|---|
| new-absent-1 | [0.47, 0.4] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-absent-2 | [0.26, 0.53] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-absent-disk-full-1 | [0.44, 0.52] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-absent-disk-full-2 | [0.13, 0.23] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-absent-disk-hang-1 | [2.73, 0.77] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-absent-disk-hang-2 | [0.69, 0.84] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-absent-held-1 | [0.32, 0.35] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| new-absent-held-2 | [1.78, 2.41] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| new-blackhole-1 | [0.05, 0.23] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 0: [] |
| new-blackhole-2 | [0.89, 4.46] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 0: [] |
| new-ended-1 | [1.37, 0.17] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-ended-2 | [1.4, 0.34] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-found-1 | [0.23, 0.18] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['Found at https://music.example.org:4681/nexus/'] / ['Found at https://music.example.org:4681/nexus/'] | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-found-2 | [2.81, 1.98] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",9,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['Found at https://music.example.org:4681/nexus/'] / ['Found at https://music.example.org:4681/nexus/'] | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-half-1 | [0.17, 0.32] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-half-2 | [1.27, 1.78] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-refused-1 | [0.4, 0.05] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-refused-2 | [1.24, 4.06] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / ['None found'] | 3 | 0: [] |
| new-saturated-1 | [2.48, 1.37] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-saturated-2 | [4.38, 0.53] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-1 | [0.18, 2.48] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-2 | [0.54, 7.26] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-3 | [2.99, 2.27] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-disk-full-1 | [0.52, 0.72] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-disk-full-2 | [0.23, 0.08] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-disk-hang-1 | [0.77, 0.44] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-disk-hang-2 | [0.84, 0.13] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | ['None found'] / no row | 3 | 3: [('GET', '/nexus/api/hello', 'NexusMusic')] |
| new-stalled-held-1 | [0.35, 2.73] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| new-stalled-held-2 | [2.41, 0.69] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-absent-1 | [4.54, 0.73] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-absent-2 | [0.72, 0.26] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-blackhole-1 | [4.2, 1.66] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-blackhole-2 | [4.06, 0.89] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-ended-1 | [1.8, 8.46] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-ended-2 | [0.53, 1.4] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-found-1 | [1.66, 1.11] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-found-2 | [4.46, 2.81] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-half-1 | [8.46, 4.94] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-half-2 | [0.39, 1.27] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-refused-1 | [0.73, 1.34] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-refused-2 | [0.53, 1.17] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-saturated-1 | [5.67, 1.8] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-saturated-2 | [7.26, 4.38] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-stalled-1 | [1.11, 5.67] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-stalled-2 | [1.98, 0.5] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |
| old-stalled-3 | [0.88, 2.64] | 0 | ['["3HrMsvEFi99qxWnytxSZkV",10,"playlist:0xUBIqKWew6szTtjwWVL0C"]'] | True | no row / no row | 0 | 0: [] |

| Run | streamed silent frames, largest step | cached silent frames, largest step | downloaded silent frames, largest step |
|---|---|---|---|
| new-absent-1 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-absent-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-absent-disk-full-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-absent-disk-full-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-absent-disk-hang-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-absent-disk-hang-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-absent-held-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-absent-held-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| new-blackhole-1 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-blackhole-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-ended-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-ended-2 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-found-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-found-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| new-half-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-half-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| new-refused-1 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-refused-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| new-saturated-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-saturated-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-stalled-1 | 1, 0.1883 | 1, 0.0305 | 1, 0.0187 |
| new-stalled-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| new-stalled-3 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-stalled-disk-full-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-stalled-disk-full-2 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-stalled-disk-hang-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-stalled-disk-hang-2 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| new-stalled-held-1 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| new-stalled-held-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| old-absent-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| old-absent-2 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| old-blackhole-1 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| old-blackhole-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| old-ended-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| old-ended-2 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| old-found-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| old-found-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0186 |
| old-half-1 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| old-half-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| old-refused-1 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |
| old-refused-2 | 1, 0.1883 | 1, 0.031 | 1, 0.0791 |
| old-saturated-1 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| old-saturated-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0186 |
| old-stalled-1 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| old-stalled-2 | 1, 0.1883 | 1, 0.0309 | 1, 0.0791 |
| old-stalled-3 | 1, 0.1883 | 1, 0.0311 | 1, 0.0791 |

| plays.log at the end of the run, times left out (song, source; bytes per line) | Runs |
|---|---|
| `[["3HrMsvEFi99qxWnytxSZkV", "playlist:0xUBIqKWew6szTtjwWVL0C"], ["1Q81m55CQ431qttIOG7WuD"], ["7fzzLozEKEAoYXRQEu33AI"], ["0F8gsygwpNvzY9VXUZjJE2"], ["3HrMsvEFi99qxWnytxSZkV"]] [71, 37, 37, 37, 37, 0]` | 46: new-absent-1, new-absent-2, new-absent-disk-full-1, new-absent-disk-full-2, new-absent-disk-hang-1, new-absent-disk-hang-2, new-absent-held-1, new-absent-held-2, new-blackhole-1, new-blackhole-2, new-ended-1, new-ended-2, new-found-1, new-found-2, new-half-1, new-half-2, new-refused-1, new-refused-2, new-saturated-1, new-saturated-2, new-stalled-1, new-stalled-2, new-stalled-3, new-stalled-disk-full-1, new-stalled-disk-full-2, new-stalled-disk-hang-1, new-stalled-disk-hang-2, new-stalled-held-1, new-stalled-held-2, old-absent-1, old-absent-2, old-blackhole-1, old-blackhole-2, old-ended-1, old-ended-2, old-found-1, old-found-2, old-half-1, old-half-2, old-refused-1, old-refused-2, old-saturated-1, old-saturated-2, old-stalled-1, old-stalled-2, old-stalled-3 |

all within: True | W1 runs: 29 | released runs: 17
