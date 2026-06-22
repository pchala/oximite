// --- Top Layout Parameters ---
top_spacing_x = 64;  
top_spacing_y = 162; 

top_off_x = top_spacing_x / 2; // 32
top_off_y = top_spacing_y / 2; // 81

// --- Bottom Layout Parameters ---
bot_hole_spacing_y = 170; 
bot_bridge_length = 84;   

bot_off_y = bot_hole_spacing_y / 2; // 85

// --- Element Parameters ---
outer_diameter = 7.3;
inner_diameter = 3.6;
cyl_height = 10; 

pylon_x = 25;
pylon_y = 20;
pylon_z = 173;

bridge_thickness = 20; 
diagonal_thickness = 10; // Reduced thickness specifically for diagonals
bridge_hole_dia = 3.6;

// New Counterbore (recess) parameters for the bottom holes
cb_diameter = 9.5;
cb_depth = 4.1;

// Center of the pylons at the bottom base
bot_off_x = (bot_bridge_length / 2) - (pylon_x / 2); 

$fn = 100; // Smooth circular edges

// ==========================================
// --- Helper Function ---
// ==========================================
pylon_top_offset_x = 5; // Outward offset for the top of the pylons on the X-axis

// Calculates the exact 3D center of a leaning pylon at any given Z height
function p_pos(qx, qy, z) = [
    qx * ((top_off_x + pylon_top_offset_x) + (bot_off_x - (top_off_x + pylon_top_offset_x)) * (-z / pylon_z)),
    qy * (top_off_y + (bot_off_y - top_off_y) * (-z / pylon_z)),
    z
];

// ==========================================
// --- Assembly ---
// ==========================================

// 1. Generate the 4 corners (Top Posts + Angled Pylons)
for (qx = [-1, 1], qy = [-1, 1]) {
    // Top cylinder post
    translate([qx * top_off_x, qy * top_off_y, 0])
        top_cylinder();
        
    // The leaning pylon
    angled_pylon(qx, qy);
}

// 2. Generate the Bridges and Diagonals on the short sides
z_bot = -pylon_z + (bridge_thickness / 2); // Z center of bottom bridge
z_mid = -pylon_z + pylon_z / 2.5;                      // Z center of middle bridge

for (qy = [-1, 1]) {
    
    // Calculate precise connection points for this side
    p_bot_left  = p_pos(-1, qy, z_bot);
    p_bot_right = p_pos( 1, qy, z_bot);
    p_mid_left  = p_pos(-0.9, qy, z_mid);
    p_mid_right = p_pos( 0.9, qy, z_mid);
    
    // Horizontal bottom bridge (has hole AND counterbore)
    strut(p_bot_left, p_bot_right, add_hole=true, is_bottom=true);
    
    // Horizontal middle bridge (has hole, NO counterbore)
    strut(p_mid_left, p_mid_right, add_hole=false, is_bottom=false);
    
}


// ==========================================
// --- Modules ---
// ==========================================

// The standard top post with the hole
module top_cylinder() {
    difference() {
        cylinder(h = cyl_height, d = outer_diameter);
        translate([0, 0, -1])
            cylinder(h = cyl_height + 2, d = inner_diameter);
    }
}

// Creates the angled vertical pillars
module angled_pylon(qx, qy) {
    top_pos = p_pos(qx, qy, 0);
    bot_pos = p_pos(qx, qy, -pylon_z);
    
    hull() {
        translate([top_pos.x, top_pos.y, -0.05])
            cube([pylon_x, pylon_y, 0.1], center = true);
            
        translate([bot_pos.x, bot_pos.y, bot_pos.z + 0.05])
            cube([pylon_x, pylon_y, 0.1], center = true);
    }
}

// Universal beam generator (connects Point A to Point B)
module strut(p1, p2, add_hole=false, is_bottom=false, thickness=bridge_thickness) {
    difference() {
        // Create the beam using hull()
        if (!is_bottom) {
        hull() {
            translate(p1) cube([pylon_x, pylon_y/1.1, thickness], center=true);
            translate(p2) cube([pylon_x, pylon_y/1.1, thickness], center=true);
        
        } } else {
        hull() {
            translate(p1) cube([pylon_x, pylon_y, thickness], center=true);
            translate(p2) cube([pylon_x, pylon_y, thickness], center=true);
        } }
        
        // Punch a vertical hole through the center of the beam
        if (add_hole) {
            mid_point = (p1 + p2) / 2;
            
            // 1. Main 3.6mm clearance hole
            translate(mid_point)
                cylinder(h = thickness + 2, d = bridge_hole_dia, center = true);
                
            // 2. The 9.5mm x 4.1mm recess (Counterbore) on the bottom
            if (is_bottom) {
                // Calculate the exact bottom face of the bridge
                // We add -0.1 to the Z start to prevent rendering glitches (Z-fighting)
                cb_z_start = mid_point.z - (thickness / 2) - 0.1;
                
                translate([mid_point.x, mid_point.y, cb_z_start])
                    // center = false means it draws from cb_z_start and goes straight UP
                    cylinder(h = cb_depth + 0.1, d = cb_diameter, center = false);
            }
        }
    }
}