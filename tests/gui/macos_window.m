// Test helper: inspect/capture/close only the application selected by the runner.
#import <AppKit/AppKit.h>
#import <ImageIO/ImageIO.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc == 3 && strcmp(argv[1], "list") == 0) {
            NSString *path = [[NSString stringWithUTF8String:argv[2]] stringByResolvingSymlinksInPath];
            NSMutableArray *result = [NSMutableArray array];
            for (NSRunningApplication *app in NSWorkspace.sharedWorkspace.runningApplications) {
                if ([app.bundleURL.path.stringByResolvingSymlinksInPath isEqualToString:path]) {
                    [result addObject:@{ @"pid": @(app.processIdentifier), @"bundle": app.bundleURL.path }];
                }
            }
            NSData *json = [NSJSONSerialization dataWithJSONObject:result options:0 error:nil];
            fwrite(json.bytes, 1, json.length, stdout);
            return 0;
        }
        if (argc == 3 && strcmp(argv[1], "quit") == 0) {
            NSRunningApplication *app = [NSRunningApplication runningApplicationWithProcessIdentifier:atoi(argv[2])];
            return app && [app terminate] ? 0 : 1;
        }
        if (argc == 3 && strcmp(argv[1], "activate") == 0) {
            NSRunningApplication *app = [NSRunningApplication runningApplicationWithProcessIdentifier:atoi(argv[2])];
            return app && [app activateWithOptions:NSApplicationActivateIgnoringOtherApps] ? 0 : 1;
        }
        if (argc != 4 || strcmp(argv[1], "snapshot") != 0) return 2;
        pid_t pid = atoi(argv[2]);
        NSArray *windows = CFBridgingRelease(CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID));
        for (NSDictionary *window in windows) {
            if ([window[(id)kCGWindowOwnerPID] intValue] != pid || [window[(id)kCGWindowLayer] intValue] != 0) continue;
            CGWindowID identifier = [window[(id)kCGWindowNumber] unsignedIntValue];
            CGImageRef image = CGWindowListCreateImage(CGRectNull, kCGWindowListOptionIncludingWindow, identifier, kCGWindowImageBoundsIgnoreFraming);
            if (!image) { fputs("window exists, but capture returned no image\n", stderr); continue; }
            NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:argv[3]]];
            CGImageDestinationRef destination = CGImageDestinationCreateWithURL((__bridge CFURLRef)url, CFSTR("public.png"), 1, NULL);
            if (!destination) { CGImageRelease(image); return 1; }
            CGImageDestinationAddImage(destination, image, NULL);
            bool saved = CGImageDestinationFinalize(destination);
            NSDictionary *result = @{ @"pid": @(pid), @"window_id": @(identifier), @"width": @(CGImageGetWidth(image)), @"height": @(CGImageGetHeight(image)) };
            CFRelease(destination); CGImageRelease(image);
            if (!saved) return 1;
            NSData *json = [NSJSONSerialization dataWithJSONObject:result options:0 error:nil];
            fwrite(json.bytes, 1, json.length, stdout);
            return 0;
        }
        fputs("no capturable on-screen window for this pid\n", stderr);
        return 1;
    }
}
